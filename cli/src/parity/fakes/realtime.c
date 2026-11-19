// The driver advances wall time at step boundaries. Kernel scheduling,
// deadlines and monotonic clocks stay real for both subjects.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static long long scripted_ms(void) {
    const char *path = getenv("PARITY_CLOCK");
    if (!path) return 0;
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return 0;
    char buf[32];
    ssize_t len = read(fd, buf, sizeof(buf)-1);
    close(fd);
    if (len <= 0) return 0;
    buf[len] = 0;
    return atoll(buf);
}
int clock_gettime(clockid_t id, struct timespec *ts) {
    if (id == CLOCK_REALTIME || id == CLOCK_REALTIME_COARSE) {
        long long ms = scripted_ms();
        if (ms) { ts->tv_sec = ms/1000; ts->tv_nsec = (ms%1000)*1000000; return 0; }
    }
    return ((int (*)(clockid_t,struct timespec *))dlsym(RTLD_NEXT,"clock_gettime"))(id,ts);
}
int gettimeofday(struct timeval *tv, void *tz) {
    long long ms = scripted_ms();
    if (ms) { tv->tv_sec = ms/1000; tv->tv_usec = (ms%1000)*1000; return 0; }
    return ((int (*)(struct timeval *,void *))dlsym(RTLD_NEXT,"gettimeofday"))(tv,tz);
}
