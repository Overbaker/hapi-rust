#include <linux/perf_event.h>
#include <sys/syscall.h>
#include <unistd.h>

int open_switch_counter(int tid) {
    struct perf_event_attr attr = {
        .size = sizeof(struct perf_event_attr),
        .type = PERF_TYPE_SOFTWARE,
        .config = PERF_COUNT_SW_CONTEXT_SWITCHES,
        .inherit = 1,
        .inherit_thread = 1,
    };
    return syscall(SYS_perf_event_open, &attr, tid, -1, -1, PERF_FLAG_FD_CLOEXEC);
}
