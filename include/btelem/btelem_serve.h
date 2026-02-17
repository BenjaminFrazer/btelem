#ifndef BTELEM_SERVE_H
#define BTELEM_SERVE_H

#include "btelem.h"
#include <stdint.h>
#include <pthread.h>

#ifdef __cplusplus
extern "C" {
#endif

#define BTELEM_SERVE_MAX_CLIENTS 16

struct btelem_client_conn {
    int                  fd;
    int                  btelem_client_id;
    struct btelem_server *server;
    pthread_t            thread;
    int                  active;
};

struct btelem_server {
    struct btelem_ctx         *ctx;
    int                       listen_fd;
    volatile int              running;
    pthread_t                 accept_thread;

    pthread_mutex_t           clients_mu;
    struct btelem_client_conn clients[BTELEM_SERVE_MAX_CLIENTS];
};

/**
 * Start a TCP trace server for the given btelem context.
 *
 * The caller owns the btelem_server struct -- it must remain valid
 * until btelem_server_stop() returns.
 *
 * Spawns an accept thread; each connection gets its own thread that
 * streams the schema (using btelem_schema_stream, no large buffer
 * required) then runs a drain-and-send loop.
 *
 * @param srv   Caller-owned server struct (zeroed before first use).
 * @param ctx   Initialised btelem context (with schema registered).
 * @param ip    Bind address (dotted quad) or NULL for INADDR_ANY.
 * @param port  TCP port to listen on.
 * @return 0 on success, -1 on failure.
 */
int btelem_serve(struct btelem_server *srv, struct btelem_ctx *ctx,
                 const char *ip, uint16_t port);

/**
 * Stop the server: close all sockets, join all threads.
 * The caller still owns the struct after this call.
 */
void btelem_server_stop(struct btelem_server *server);

#ifdef __cplusplus
}
#endif

#endif /* BTELEM_SERVE_H */
