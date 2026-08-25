/*
 * Native network interposer for scripts/release_offline_audit.py.
 *
 * This library is deliberately tiny and dependency-free.  It records and denies
 * outbound IPv4/IPv6 operations made through the platform C library while leaving
 * Unix-domain IPC and inbound loopback reader traffic alone.  The audit harness
 * treats any record as a failure.
 */

#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef O_CLOEXEC
#define O_CLOEXEC 0
#endif

static void record_attempt(const char *operation, int family) {
    const char *path = getenv("WIKISYNC_OFFLINE_AUDIT_LOG");
    char line[96];
    size_t operation_length;
    size_t position = 0;
    const char *family_name;
    int descriptor;

    if (path == NULL || path[0] == '\0') {
        return;
    }
    if (family == AF_INET) {
        family_name = "AF_INET";
    } else if (family == AF_INET6) {
        family_name = "AF_INET6";
    } else {
        family_name = "UNKNOWN";
    }
    operation_length = strlen(operation);
    if (operation_length + strlen(family_name) + 2 >= sizeof(line)) {
        return;
    }
    memcpy(line + position, operation, operation_length);
    position += operation_length;
    line[position++] = ' ';
    memcpy(line + position, family_name, strlen(family_name));
    position += strlen(family_name);
    line[position++] = '\n';

    descriptor = open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0600);
    if (descriptor >= 0) {
        (void)write(descriptor, line, position);
        (void)close(descriptor);
    }
}

static int is_network_family(int family) {
    return family == AF_INET || family == AF_INET6;
}

static int audit_connect(int socket_descriptor, const struct sockaddr *address,
                         socklen_t address_length) {
#ifndef __APPLE__
    typedef int (*connect_fn)(int, const struct sockaddr *, socklen_t);
    static connect_fn real_connect;
#endif

    if (address != NULL && is_network_family(address->sa_family)) {
        record_attempt("connect", address->sa_family);
        errno = ENETUNREACH;
        return -1;
    }
#ifdef __APPLE__
    return connect(socket_descriptor, address, address_length);
#else
    if (real_connect == NULL) {
        real_connect = (connect_fn)dlsym(RTLD_NEXT, "connect");
    }
    if (real_connect == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return real_connect(socket_descriptor, address, address_length);
#endif
}

static ssize_t audit_sendto(int socket_descriptor, const void *buffer, size_t length, int flags,
                            const struct sockaddr *destination,
                            socklen_t destination_length) {
#ifndef __APPLE__
    typedef ssize_t (*sendto_fn)(int, const void *, size_t, int, const struct sockaddr *,
                                 socklen_t);
    static sendto_fn real_sendto;
#endif

    if (destination != NULL && is_network_family(destination->sa_family)) {
        record_attempt("sendto", destination->sa_family);
        errno = ENETUNREACH;
        return -1;
    }
#ifdef __APPLE__
    return sendto(socket_descriptor, buffer, length, flags, destination, destination_length);
#else
    if (real_sendto == NULL) {
        real_sendto = (sendto_fn)dlsym(RTLD_NEXT, "sendto");
    }
    if (real_sendto == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return real_sendto(socket_descriptor, buffer, length, flags, destination,
                       destination_length);
#endif
}

static ssize_t audit_sendmsg(int socket_descriptor, const struct msghdr *message, int flags) {
#ifndef __APPLE__
    typedef ssize_t (*sendmsg_fn)(int, const struct msghdr *, int);
    static sendmsg_fn real_sendmsg;
#endif
    const struct sockaddr *destination = NULL;

    if (message != NULL && message->msg_name != NULL &&
        message->msg_namelen >= sizeof(sa_family_t)) {
        destination = (const struct sockaddr *)message->msg_name;
    }
    if (destination != NULL && is_network_family(destination->sa_family)) {
        record_attempt("sendmsg", destination->sa_family);
        errno = ENETUNREACH;
        return -1;
    }
#ifdef __APPLE__
    return sendmsg(socket_descriptor, message, flags);
#else
    if (real_sendmsg == NULL) {
        real_sendmsg = (sendmsg_fn)dlsym(RTLD_NEXT, "sendmsg");
    }
    if (real_sendmsg == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return real_sendmsg(socket_descriptor, message, flags);
#endif
}

static int audit_getaddrinfo(const char *node, const char *service, const struct addrinfo *hints,
                             struct addrinfo **result) {
#ifndef __APPLE__
    typedef int (*getaddrinfo_fn)(const char *, const char *, const struct addrinfo *,
                                  struct addrinfo **);
    static getaddrinfo_fn real_getaddrinfo;
#endif

    if (node != NULL && node[0] != '\0') {
        int family = hints == NULL ? AF_UNSPEC : hints->ai_family;
        record_attempt("getaddrinfo", family);
        return EAI_AGAIN;
    }
#ifdef __APPLE__
    return getaddrinfo(node, service, hints, result);
#else
    if (real_getaddrinfo == NULL) {
        real_getaddrinfo = (getaddrinfo_fn)dlsym(RTLD_NEXT, "getaddrinfo");
    }
    if (real_getaddrinfo == NULL) {
        return EAI_SYSTEM;
    }
    return real_getaddrinfo(node, service, hints, result);
#endif
}

#ifdef __APPLE__
#define INTERPOSE(replacement, replacee)                                                     \
    __attribute__((used)) static struct {                                                    \
        const void *replacement;                                                             \
        const void *replacee;                                                                \
    } interpose_##replacee __attribute__((section("__DATA,__interpose"))) = {                \
        (const void *)(uintptr_t)&replacement, (const void *)(uintptr_t)&replacee             \
    }

INTERPOSE(audit_connect, connect);
INTERPOSE(audit_sendto, sendto);
INTERPOSE(audit_sendmsg, sendmsg);
INTERPOSE(audit_getaddrinfo, getaddrinfo);
#else
int connect(int socket_descriptor, const struct sockaddr *address, socklen_t address_length) {
    return audit_connect(socket_descriptor, address, address_length);
}

ssize_t sendto(int socket_descriptor, const void *buffer, size_t length, int flags,
               const struct sockaddr *destination, socklen_t destination_length) {
    return audit_sendto(socket_descriptor, buffer, length, flags, destination,
                        destination_length);
}

ssize_t sendmsg(int socket_descriptor, const struct msghdr *message, int flags) {
    return audit_sendmsg(socket_descriptor, message, flags);
}

int getaddrinfo(const char *node, const char *service, const struct addrinfo *hints,
                struct addrinfo **result) {
    return audit_getaddrinfo(node, service, hints, result);
}
#endif
