/**
 * btelem basic example — continuous TCP telemetry source
 *
 * Generates synthetic sensor + motor telemetry at 50 Hz and serves it
 * over TCP on localhost:4040.  Runs until killed (Ctrl-C).
 *
 * Connect with:  btelem live --tcp localhost:4040
 */

#include <math.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "btelem/btelem_serve.h"

#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif

/* -------------------------------------------------------------------------
 * 1. Telemetry structs
 * ---------------------------------------------------------------------- */

struct sensor_data {
    float temperature;
    float pressure;
    float humidity;
};

struct motor_state {
    float rpm;
    float current;
};

struct imu_data {
    float accel[3];  /* x, y, z  (m/s²) */
    float gyro[3];   /* x, y, z  (rad/s) */
};

struct system_status {
    uint8_t state;   /* enum: IDLE, STARTING, RUNNING, STOPPING, FAULT */
};

struct gpio_state {
    uint32_t flags;  /* bitfield: enabled(1), error(1), mode(2), channel(4), priority(3), seq(8), active(1) */
};

/* -------------------------------------------------------------------------
 * 2. Schema
 * ---------------------------------------------------------------------- */

static const struct btelem_field_def sensor_fields[] = {
    BTELEM_FIELD(struct sensor_data, temperature, BTELEM_F32),
    BTELEM_FIELD(struct sensor_data, pressure,    BTELEM_F32),
    BTELEM_FIELD(struct sensor_data, humidity,    BTELEM_F32),
};
BTELEM_SCHEMA_ENTRY(SENSOR, 0, "sensor_data", "Environmental sensors",
                    struct sensor_data, sensor_fields);

static const struct btelem_field_def motor_fields[] = {
    BTELEM_FIELD(struct motor_state, rpm,     BTELEM_F32),
    BTELEM_FIELD(struct motor_state, current, BTELEM_F32),
};
BTELEM_SCHEMA_ENTRY(MOTOR, 1, "motor_state", "Motor controller",
                    struct motor_state, motor_fields);

static const struct btelem_field_def imu_fields[] = {
    BTELEM_ARRAY_FIELD(struct imu_data, accel, BTELEM_F32, 3),
    BTELEM_ARRAY_FIELD(struct imu_data, gyro,  BTELEM_F32, 3),
};
BTELEM_SCHEMA_ENTRY(IMU, 3, "imu_data", "Inertial measurement unit",
                    struct imu_data, imu_fields);

static const char *system_state_labels[] = {
    "IDLE", "STARTING", "RUNNING", "STOPPING", "FAULT"
};
BTELEM_ENUM_DEF(system_state, system_state_labels);

static const struct btelem_field_def status_fields[] = {
    BTELEM_FIELD_ENUM(struct system_status, state, system_state),
};
BTELEM_SCHEMA_ENTRY(STATUS, 2, "system_status", "System state machine",
                    struct system_status, status_fields);

static const struct btelem_bit_def gpio_bits[] = {
    BTELEM_BIT("enabled",  0, 1),
    BTELEM_BIT("error",    1, 1),
    BTELEM_BIT("mode",     2, 2),
    BTELEM_BIT("channel",  4, 4),
    BTELEM_BIT("priority", 16, 3),
    BTELEM_BIT("seq",      19, 8),
    BTELEM_BIT("active",   27, 1),
};
BTELEM_BITFIELD_DEF(gpio_flags, gpio_bits);

static const struct btelem_field_def gpio_fields[] = {
    BTELEM_FIELD_BITFIELD(struct gpio_state, flags, gpio_flags),
};
BTELEM_SCHEMA_ENTRY(GPIO, 4, "gpio_state", "GPIO pin status",
                    struct gpio_state, gpio_fields);

/* -------------------------------------------------------------------------
 * 3. Signal handling
 * ---------------------------------------------------------------------- */

static volatile sig_atomic_t running = 1;

static void handle_signal(int sig)
{
    (void)sig;
    running = 0;
}

/* -------------------------------------------------------------------------
 * 4. Synthetic data (matches tcp_source.py waveforms)
 * ---------------------------------------------------------------------- */

static float randf(void)
{
    return (float)rand() / (float)RAND_MAX;
}

/* Box-Muller for Gaussian noise */
static float gauss(float sigma)
{
    float u1 = randf() + 1e-10f;
    float u2 = randf();
    return sigma * sqrtf(-2.0f * logf(u1)) * cosf(2.0f * (float)M_PI * u2);
}

static void log_telemetry(struct btelem_ctx *ctx, double t)
{
    /* sensor_data: slow sine waves + noise */
    struct sensor_data s = {
        .temperature = 22.0f + 5.0f * sinf(2.0f * (float)M_PI * (float)t / 10.0f)
                       + gauss(0.3f),
        .pressure    = 1013.0f + 20.0f * sinf(2.0f * (float)M_PI * (float)t / 30.0f)
                       + gauss(1.0f),
        .humidity    = 50.0f + 15.0f * sinf(2.0f * (float)M_PI * (float)t / 20.0f)
                       + gauss(0.5f),
    };
    BTELEM_LOG(ctx, SENSOR, s);

    /* motor_state: ramp + triangle wave */
    struct motor_state m = {
        .rpm     = 1500.0f + 500.0f * sinf(2.0f * (float)M_PI * (float)t / 8.0f),
        .current = 2.0f + 1.0f * fabsf(fmodf((float)t, 4.0f) - 2.0f)
                   + gauss(0.1f),
    };
    BTELEM_LOG(ctx, MOTOR, m);

    /* imu_data: accelerometer + gyroscope with gravity + vibration */
    struct imu_data imu = {
        .accel = {
            0.5f * sinf(2.0f * (float)M_PI * (float)t / 6.0f) + gauss(0.05f),
            0.3f * cosf(2.0f * (float)M_PI * (float)t / 8.0f) + gauss(0.05f),
            9.81f + 0.2f * sinf(2.0f * (float)M_PI * (float)t / 4.0f) + gauss(0.05f),
        },
        .gyro = {
            0.1f * sinf(2.0f * (float)M_PI * (float)t / 5.0f) + gauss(0.01f),
            0.15f * cosf(2.0f * (float)M_PI * (float)t / 7.0f) + gauss(0.01f),
            0.05f * sinf(2.0f * (float)M_PI * (float)t / 3.0f) + gauss(0.01f),
        },
    };
    BTELEM_LOG(ctx, IMU, imu);

    /* system_status: cycle through states every 2 seconds.
     * Values 0-4 map to named labels; 5-7 are intentionally unnamed
     * to exercise unknown-enum-value display in the viewer. */
    int phase = (int)(t / 2.0) % 8;
    struct system_status st = { .state = (uint8_t)phase };
    BTELEM_LOG(ctx, STATUS, st);

    /* gpio_state: bitfield with rotating flags */
    uint32_t gf = 0;
    gf |= ((int)(t * 2.0) & 1);              /* enabled: toggles at 2 Hz */
    gf |= (((int)(t / 5.0) & 1) << 1);       /* error: toggles every 5s */
    gf |= (((int)(t / 3.0) % 4) << 2);       /* mode: cycles 0-3 */
    gf |= (((int)(t) % 16) << 4);             /* channel: cycles 0-15 */
    gf |= (((uint32_t)((int)(t) % 8)) << 16);       /* priority: cycles 0-7 */
    gf |= (((uint32_t)((int)(t * 10.0) % 256)) << 19); /* seq: cycles 0-255 */
    gf |= (((uint32_t)((int)(t / 4.0) & 1)) << 27);    /* active: toggles every 4s */
    struct gpio_state gs = { .flags = gf };
    BTELEM_LOG(ctx, GPIO, gs);
}

/* -------------------------------------------------------------------------
 * 5. Main
 * ---------------------------------------------------------------------- */

#define RATE_HZ 10000
#define PORT    4040

int main(void)
{
    signal(SIGINT,  handle_signal);
    signal(SIGTERM, handle_signal);
    srand((unsigned)time(NULL));

    /* Allocate ring buffer */
    uint32_t ring_entries = 16384;
    size_t ring_sz = btelem_ring_size(ring_entries);
    void *ring_mem = malloc(ring_sz);
    if (!ring_mem) {
        fprintf(stderr, "failed to allocate ring buffer\n");
        return 1;
    }

    struct btelem_ctx ctx;
    btelem_init(&ctx, ring_mem, ring_entries);
    btelem_register(&ctx, &btelem_schema_SENSOR);
    btelem_register(&ctx, &btelem_schema_MOTOR);
    btelem_register(&ctx, &btelem_schema_IMU);
    btelem_register(&ctx, &btelem_schema_STATUS);
    btelem_register(&ctx, &btelem_schema_GPIO);

    static struct btelem_server srv;
    memset(&srv, 0, sizeof(srv));

    if (btelem_serve(&srv, &ctx, "0.0.0.0", PORT) < 0) {
        fprintf(stderr, "failed to start server on port %d\n", PORT);
        free(ring_mem);
        return 1;
    }

    printf("Serving telemetry on 0.0.0.0:%d at %d Hz  (Ctrl-C to stop)\n",
           PORT, RATE_HZ);
    printf("  btelem-viewer --live tcp:localhost:%d\n", PORT);

    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    uint64_t seq = 0;

    while (running) {
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        double t = (double)(now.tv_sec - t0.tv_sec)
                 + (double)(now.tv_nsec - t0.tv_nsec) / 1e9;

        log_telemetry(&ctx, t);
        seq++;

        if (seq % RATE_HZ == 0)
            printf("  %lu packets (%.1fs)\n", (unsigned long)seq, t);

        usleep(1000000 / RATE_HZ);
    }

    printf("\nShutting down...\n");
    btelem_server_stop(&srv);
    free(ring_mem);

    return 0;
}
