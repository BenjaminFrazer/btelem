#include "btelem/btelem_serve.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <netinet/in.h>
#include <netinet/tcp.h>

#define BTELEM_SERVE_PKT_BUF    65536

/* TCP keepalive parameters for shedding viewers that vanished without
 * sending FIN (e.g. cable yanked, host crash).  Tuned for "notice within
 * ~25s" which is plenty for a developer-facing telemetry stream. */
#define BTELEM_KEEPALIVE_IDLE_S    10
#define BTELEM_KEEPALIVE_INTVL_S   5
#define BTELEM_KEEPALIVE_PROBES    3

/* Returns 1 if the peer has closed the connection (FIN received), 0 if the
 * socket still looks healthy, -1 on any other error.  Non-blocking MSG_PEEK
 * does not consume data — important because the server never reads from
 * client sockets in normal operation. */
static int peer_closed(int fd)
{
    uint8_t b;
    ssize_t n = recv(fd, &b, 1, MSG_PEEK | MSG_DONTWAIT);
    if (n == 0)
        return 1; /* orderly close: peer sent FIN */
    if (n < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)
            return 0; /* no data, no FIN — connection still up */
        return -1; /* ECONNRESET, etc. — treat as closed by caller */
    }
    return 0; /* peer sent unexpected data — connection still up */
}

/* --------------------------------------------------------------------------
 * Helpers
 * ----------------------------------------------------------------------- */

static int send_all(int fd, const void *data, size_t len)
{
    const uint8_t *p = (const uint8_t *)data;
    while (len > 0) {
        ssize_t n = write(fd, p, len);
        if (n <= 0) {
            int e = errno;
            if (e == EAGAIN || e == EWOULDBLOCK) {
                fprintf(stderr, "btelem_serve: write() timed out (fd=%d, remaining=%zu), retrying...\n",
                        fd, len);
                continue;
            }
            fprintf(stderr, "btelem_serve: write() failed: %s (errno=%d, fd=%d, len=%zu)\n",
                    strerror(e), e, fd, len);
            return -1;
        }
        p += (size_t)n;
        len -= (size_t)n;
    }
    return 0;
}

/* Callback context for streaming schema over a socket */
struct schema_send_ctx {
    int fd;
    int error;
};

static int schema_send_chunk(const void *chunk, size_t len, void *user)
{
    struct schema_send_ctx *sc = (struct schema_send_ctx *)user;
    if (send_all(sc->fd, chunk, len) < 0) {
        sc->error = 1;
        return -1;
    }
    return 0;
}

/* --------------------------------------------------------------------------
 * Client thread: stream schema, then drain loop
 * ----------------------------------------------------------------------- */

static void *client_thread(void *arg)
{
    struct btelem_client_conn *conn = (struct btelem_client_conn *)arg;
    struct btelem_server *srv = conn->server;
    struct btelem_ctx *ctx = srv->ctx;
    uint8_t pkt_buf[BTELEM_SERVE_PKT_BUF];

    /* Send length-prefixed schema, streamed chunk-by-chunk */
    int schema_size = btelem_schema_serialize(ctx, NULL, 0);
    if (schema_size > 0) {
        uint32_t slen = (uint32_t)schema_size;
        if (send_all(conn->fd, &slen, 4) < 0)
            goto done;

        struct schema_send_ctx sc = { .fd = conn->fd, .error = 0 };
        btelem_schema_stream(ctx, schema_send_chunk, &sc);
        if (sc.error)
            goto done;
    }

    /* Drain loop: send length-prefixed packed batches */

    /* Set send timeout so write() can't block forever when the receiver
     * stalls (e.g. viewer backgrounded).  If we can't push data within
     * this window we disconnect the slow client instead of wedging the
     * drain thread.  The viewer will need to reconnect. */
    {
        struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
        setsockopt(conn->fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
    }

    fprintf(stderr, "btelem_serve: client %d connected (fd=%d)\n",
            conn->btelem_client_id, conn->fd);

    uint64_t total_bytes = 0;
    uint64_t total_pkts = 0;
    uint64_t total_dropped = 0;
    uint64_t empty_drains = 0;
    uint64_t last_report_pkts = 0;
    uint64_t last_report_dropped = 0;
    struct timespec last_report;
    clock_gettime(CLOCK_MONOTONIC, &last_report);

    while (srv->running) {
        int n = btelem_drain_packed(ctx, conn->btelem_client_id,
                                    pkt_buf, sizeof(pkt_buf));
        if (n > 0) {
            uint32_t plen = (uint32_t)n;
            if (send_all(conn->fd, &plen, 4) < 0 ||
                send_all(conn->fd, pkt_buf, plen) < 0) {
                fprintf(stderr, "btelem_serve: client %d send failed after "
                        "%lu pkts / %lu bytes — disconnecting\n",
                        conn->btelem_client_id,
                        (unsigned long)total_pkts, (unsigned long)total_bytes);
                break;
            }
            total_bytes += 4 + plen;
            total_pkts++;
            const struct btelem_packet_header *ph =
                (const struct btelem_packet_header *)pkt_buf;
            total_dropped += ph->dropped;
            empty_drains = 0;
        } else {
            empty_drains++;
            /* Cheap liveness probe: detect viewers that quietly went away
             * without sending us data we'd otherwise stumble over.  Without
             * this the drain loop happily spins forever and the slot is
             * never reclaimed. */
            if ((empty_drains & 0xFF) == 0) {
                int pc = peer_closed(conn->fd);
                if (pc != 0) {
                    fprintf(stderr, "btelem_serve: client %d peer gone "
                            "(peer_closed=%d) — disconnecting\n",
                            conn->btelem_client_id, pc);
                    break;
                }
            }
            usleep(1000);
        }

        /* Periodic status every 2 seconds */
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        double dt = (double)(now.tv_sec - last_report.tv_sec)
                  + (double)(now.tv_nsec - last_report.tv_nsec) / 1e9;
        if (dt >= 2.0) {
            uint64_t delta_pkts = total_pkts - last_report_pkts;
            uint64_t delta_dropped = total_dropped - last_report_dropped;
            fprintf(stderr, "btelem_serve: client %d status: %lu pkts (+%lu) "
                    "%lu bytes, dropped=%lu (+%lu), empty_drains=%lu\n",
                    conn->btelem_client_id,
                    (unsigned long)total_pkts, (unsigned long)delta_pkts,
                    (unsigned long)total_bytes,
                    (unsigned long)total_dropped, (unsigned long)delta_dropped,
                    (unsigned long)empty_drains);
            last_report = now;
            last_report_pkts = total_pkts;
            last_report_dropped = total_dropped;
            empty_drains = 0;
        }
    }

    if (!srv->running)
        fprintf(stderr, "btelem_serve: client %d exiting (server stopping)\n",
                conn->btelem_client_id);

done:
    close(conn->fd);
    conn->fd = -1;
    btelem_client_close(ctx, conn->btelem_client_id);

    pthread_mutex_lock(&srv->clients_mu);
    conn->active = 0;
    pthread_mutex_unlock(&srv->clients_mu);

    return NULL;
}

/* --------------------------------------------------------------------------
 * Accept thread
 * ----------------------------------------------------------------------- */

static void *accept_thread(void *arg)
{
    struct btelem_server *srv = (struct btelem_server *)arg;

    while (srv->running) {
        int fd = accept(srv->listen_fd, NULL, NULL);
        if (fd < 0)
            break;

        /* Enable TCP keepalive so the kernel notices half-open connections
         * even when we have nothing to write (idle drain loop). */
        {
            int one = 1;
            int idle = BTELEM_KEEPALIVE_IDLE_S;
            int intvl = BTELEM_KEEPALIVE_INTVL_S;
            int cnt = BTELEM_KEEPALIVE_PROBES;
            setsockopt(fd, SOL_SOCKET,  SO_KEEPALIVE,  &one,   sizeof(one));
            setsockopt(fd, IPPROTO_TCP, TCP_KEEPIDLE,  &idle,  sizeof(idle));
            setsockopt(fd, IPPROTO_TCP, TCP_KEEPINTVL, &intvl, sizeof(intvl));
            setsockopt(fd, IPPROTO_TCP, TCP_KEEPCNT,   &cnt,   sizeof(cnt));
        }

        int btelem_cid = btelem_client_open(srv->ctx, NULL, 0);
        if (btelem_cid < 0) {
            fprintf(stderr, "btelem_serve: refusing client — "
                    "btelem_client_open() failed "
                    "(all %d btelem client slots in use)\n",
                    BTELEM_MAX_CLIENTS);
            close(fd);
            continue;
        }

        pthread_mutex_lock(&srv->clients_mu);

        int slot = -1;
        for (int i = 0; i < BTELEM_SERVE_MAX_CLIENTS; i++) {
            if (!srv->clients[i].active) {
                slot = i;
                break;
            }
        }

        if (slot < 0) {
            /* No free slots */
            pthread_mutex_unlock(&srv->clients_mu);
            btelem_client_close(srv->ctx, btelem_cid);
            close(fd);
            continue;
        }

        struct btelem_client_conn *conn = &srv->clients[slot];
        conn->fd = fd;
        conn->btelem_client_id = btelem_cid;
        conn->server = srv;
        conn->active = 1;

        pthread_create(&conn->thread, NULL, client_thread, conn);
        pthread_detach(conn->thread);

        pthread_mutex_unlock(&srv->clients_mu);
    }

    return NULL;
}

/* --------------------------------------------------------------------------
 * Public API
 * ----------------------------------------------------------------------- */

int btelem_serve(struct btelem_server *srv, struct btelem_ctx *ctx,
                 const char *ip, uint16_t port)
{
    if (!srv || !ctx)
        return -1;

    int lsock = socket(AF_INET, SOCK_STREAM, 0);
    if (lsock < 0)
        return -1;

    int opt = 1;
    setsockopt(lsock, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);

    if (ip)
        inet_pton(AF_INET, ip, &addr.sin_addr);
    else
        addr.sin_addr.s_addr = htonl(INADDR_ANY);

    if (bind(lsock, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        close(lsock);
        return -1;
    }

    if (listen(lsock, 8) < 0) {
        close(lsock);
        return -1;
    }

    srv->ctx = ctx;
    srv->listen_fd = lsock;
    srv->running = 1;
    pthread_mutex_init(&srv->clients_mu, NULL);

    for (int i = 0; i < BTELEM_SERVE_MAX_CLIENTS; i++) {
        srv->clients[i].fd = -1;
        srv->clients[i].active = 0;
    }

    if (pthread_create(&srv->accept_thread, NULL, accept_thread, srv) != 0) {
        close(lsock);
        pthread_mutex_destroy(&srv->clients_mu);
        return -1;
    }

    return 0;
}

void btelem_server_stop(struct btelem_server *server)
{
    if (!server || !server->running)
        return;

    server->running = 0;

    /* Shutdown + close listen socket to unblock accept() */
    shutdown(server->listen_fd, SHUT_RDWR);
    close(server->listen_fd);

    /* Join accept thread */
    pthread_join(server->accept_thread, NULL);

    /* Close all client connections to unblock their write()/usleep() */
    pthread_mutex_lock(&server->clients_mu);
    for (int i = 0; i < BTELEM_SERVE_MAX_CLIENTS; i++) {
        if (server->clients[i].active && server->clients[i].fd >= 0) {
            shutdown(server->clients[i].fd, SHUT_RDWR);
        }
    }
    pthread_mutex_unlock(&server->clients_mu);

    /* Wait for all client threads to finish */
    for (int i = 0; i < 100; i++) {
        int any_active = 0;
        pthread_mutex_lock(&server->clients_mu);
        for (int j = 0; j < BTELEM_SERVE_MAX_CLIENTS; j++) {
            if (server->clients[j].active) {
                any_active = 1;
                break;
            }
        }
        pthread_mutex_unlock(&server->clients_mu);
        if (!any_active)
            break;
        usleep(10000);
    }

    pthread_mutex_destroy(&server->clients_mu);
}
