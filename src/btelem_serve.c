#include "btelem/btelem_serve.h"

#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <arpa/inet.h>
#include <sys/socket.h>

#define BTELEM_SERVE_MAX_CLIENTS 16
#define BTELEM_SERVE_PKT_BUF    65536

struct client_conn {
    int                  fd;
    int                  btelem_client_id;
    struct btelem_server *server;
    pthread_t            thread;
    int                  active;
};

struct btelem_server {
    struct btelem_ctx    *ctx;
    int                  listen_fd;
    volatile int         running;
    pthread_t            accept_thread;

    pthread_mutex_t      clients_mu;
    struct client_conn   clients[BTELEM_SERVE_MAX_CLIENTS];
};

/* --------------------------------------------------------------------------
 * Helpers
 * ----------------------------------------------------------------------- */

static int send_all(int fd, const void *data, size_t len)
{
    const uint8_t *p = (const uint8_t *)data;
    while (len > 0) {
        ssize_t n = write(fd, p, len);
        if (n <= 0)
            return -1;
        p += (size_t)n;
        len -= (size_t)n;
    }
    return 0;
}

/* --------------------------------------------------------------------------
 * Client thread: send schema, then drain loop
 * ----------------------------------------------------------------------- */

static void *client_thread(void *arg)
{
    struct client_conn *conn = (struct client_conn *)arg;
    struct btelem_server *srv = conn->server;
    struct btelem_ctx *ctx = srv->ctx;
    uint8_t pkt_buf[BTELEM_SERVE_PKT_BUF];

    /* Send length-prefixed schema blob */
    {
        uint8_t schema_buf[8192];
        int slen = btelem_schema_serialize(ctx, schema_buf, sizeof(schema_buf));
        if (slen > 0) {
            uint32_t schema_len = (uint32_t)slen;
            if (send_all(conn->fd, &schema_len, 4) < 0 ||
                send_all(conn->fd, schema_buf, schema_len) < 0)
                goto done;
        }
    }

    /* Drain loop: send length-prefixed packed batches */

    while (srv->running) {
        int n = btelem_drain_packed(ctx, conn->btelem_client_id,
                                    pkt_buf, sizeof(pkt_buf));
        if (n > 0) {
            uint32_t plen = (uint32_t)n;
            if (send_all(conn->fd, &plen, 4) < 0 ||
                send_all(conn->fd, pkt_buf, plen) < 0)
                break;
        } else {
            usleep(1000);
        }
    }

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

        int btelem_cid = btelem_client_open(srv->ctx, 0);
        if (btelem_cid < 0) {
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

        struct client_conn *conn = &srv->clients[slot];
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

struct btelem_server *btelem_serve(struct btelem_ctx *ctx,
                                   const char *ip, uint16_t port)
{
    if (!ctx)
        return NULL;

    int lsock = socket(AF_INET, SOCK_STREAM, 0);
    if (lsock < 0)
        return NULL;

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
        return NULL;
    }

    if (listen(lsock, 8) < 0) {
        close(lsock);
        return NULL;
    }

    struct btelem_server *srv = calloc(1, sizeof(*srv));
    if (!srv) {
        close(lsock);
        return NULL;
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
        free(srv);
        return NULL;
    }

    return srv;
}

void btelem_server_stop(struct btelem_server *server)
{
    if (!server)
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
    free(server);
}
