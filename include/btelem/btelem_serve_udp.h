#ifndef BTELEM_SERVE_UDP_H
#define BTELEM_SERVE_UDP_H

#include "btelem.h"
#include <stdint.h>
#include <pthread.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * UDP JSON telemetry server for PlotJuggler compatibility.
 *
 * Drains entries from a btelem context, encodes each as a newline-
 * delimited JSON object, and sends it as a UDP datagram.
 *
 * Field naming uses dot-separated paths for PlotJuggler's tree view:
 *   "entry_name.field_name"            — scalar fields
 *   "entry_name.field_name_N"          — array elements (0-indexed)
 *   "entry_name.field_name.bit_name"   — bitfield sub-fields
 *
 * Timestamp is emitted as "timestamp" in seconds (float64).
 */

struct btelem_udp_server {
    struct btelem_ctx  *ctx;
    int                 sock_fd;
    int                 btelem_client_id;
    volatile int        running;
    pthread_t           drain_thread;

    /* Destination address */
    char                dest_ip[64];
    uint16_t            dest_port;
};

/**
 * Start a UDP JSON server for the given btelem context.
 *
 * Spawns a drain thread that reads entries from the ring buffer,
 * encodes them as JSON, and sends UDP datagrams to dest_ip:dest_port.
 *
 * @param srv        Caller-owned server struct (zeroed before first use).
 * @param ctx        Initialised btelem context (with schema registered).
 * @param dest_ip    Destination IP (dotted quad), e.g. "127.0.0.1".
 * @param dest_port  Destination UDP port (PlotJuggler default: 9870).
 * @return 0 on success, -1 on failure.
 */
int btelem_serve_udp(struct btelem_udp_server *srv, struct btelem_ctx *ctx,
                     const char *dest_ip, uint16_t dest_port);

/**
 * Stop the UDP server: close socket, join drain thread.
 */
void btelem_udp_server_stop(struct btelem_udp_server *srv);

#ifdef __cplusplus
}
#endif

#endif /* BTELEM_SERVE_UDP_H */
