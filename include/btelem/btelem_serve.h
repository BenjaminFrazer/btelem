#ifndef BTELEM_SERVE_H
#define BTELEM_SERVE_H

#include "btelem.h"
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

struct btelem_server;

/**
 * Start a TCP trace server for the given btelem context.
 *
 * Spawns an accept thread; each connection gets its own thread that
 * sends the schema then runs a drain-and-send loop.
 *
 * @param ctx   Initialised btelem context (with schema registered).
 * @param ip    Bind address (dotted quad) or NULL for INADDR_ANY.
 * @param port  TCP port to listen on.
 * @return Opaque server handle, or NULL on failure.
 */
struct btelem_server *btelem_serve(struct btelem_ctx *ctx,
                                   const char *ip, uint16_t port);

/**
 * Stop the server: close all sockets, join all threads, free resources.
 */
void btelem_server_stop(struct btelem_server *server);

#ifdef __cplusplus
}
#endif

#endif /* BTELEM_SERVE_H */
