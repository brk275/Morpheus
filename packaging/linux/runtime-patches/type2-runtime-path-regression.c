/* SPDX-License-Identifier: AGPL-3.0-only
 * Exercise the exact patched runtime source. All path probes are intercepted;
 * no directory, mount, payload extraction or application execution is allowed.
 * Owned pipe/unlinked-file cases additionally check the post-wait observer.
 */
#define main morpheus_runtime_main
#define readlink morpheus_fixture_readlink
#define mkdir morpheus_fixture_mkdir
#define access morpheus_fixture_access
#define getenv morpheus_fixture_getenv
#include "runtime.c"
#undef main
#undef readlink
#undef mkdir
#undef access
#undef getenv

#include <sys/resource.h>
#include <sys/prctl.h>

enum fixture_mode { HOME, CONFIG, HELP, STARTUP, IMPLICIT };
static enum fixture_mode fixture_mode;
static ssize_t fixture_length;
static int readlink_calls, mkdir_calls, access_calls;
static int expected_readlink, expected_mkdir, expected_access;

static void verify_calls(void) {
    if (readlink_calls != expected_readlink || mkdir_calls != expected_mkdir || access_calls != expected_access)
        _Exit(90);
}

ssize_t morpheus_fixture_readlink(const char *path, char *buffer, size_t capacity) {
    if ((strcmp(path, "/synthetic/runtime") && strcmp(path, "/proc/self/exe")) || capacity != PATH_MAX)
        _Exit(91);
    readlink_calls++;
    if (fixture_length < 0) {
        errno = EIO;
        return -1;
    }
    size_t count = (size_t) fixture_length < capacity ? (size_t) fixture_length : capacity;
    memset(buffer, 'x', count);
    return (ssize_t) count;
}

int morpheus_fixture_mkdir(const char *path, mode_t mode) {
    const char *suffix = fixture_mode == HOME ? ".home" : ".config";
    if (!expected_mkdir || fixture_length < 0 || mode != S_IRWXU
            || strnlen(path, PATH_MAX) != (size_t) fixture_length + strlen(suffix)
            || strcmp(path + fixture_length, suffix))
        _Exit(92);
    mkdir_calls++;
    return 0;
}

int morpheus_fixture_access(const char *path, int mode) {
    const char *suffix = access_calls == 0 ? ".home" : ".config";
    if (fixture_mode != IMPLICIT || mode != W_OK || access_calls >= expected_access
            || strnlen(path, PATH_MAX) != (size_t) fixture_length + strlen(suffix)
            || strcmp(path + fixture_length, suffix))
        _Exit(93);
    access_calls++;
    errno = ENOENT;
    return -1;
}

char *morpheus_fixture_getenv(const char *name) {
    /* Stop the main path immediately after its accepted readlink and NUL,
     * before extraction, portable-directory probes, mount or exec. */
    if (fixture_mode == STARTUP && readlink_calls == 1 && !strcmp(name, "APPIMAGE_EXTRACT_AND_RUN"))
        exit(0);
    return NULL;
}

struct fixture_case {
    enum fixture_mode mode;
    ssize_t length;
    int status;
    int access;
};

/* Actual production rendering with real owned-child wait statuses. */
static int wait_observation_cases(void) {
    const struct { int code; int signal; } cases[] = {
        {0, 0}, {1, 0}, {126, 0}, {127, 0}, {255, 0},
        {0, SIGTERM}, {0, SIGABRT}, {0, SIGSEGV}
    };
    const char *cleanup_names[] = {"skipped", "passed", "failed"};
    for (size_t index = 0; index < sizeof(cases) / sizeof(cases[0]); index++) {
        pid_t child = fork();
        if (child == -1)
            return 1;
        if (child == 0) {
            struct rlimit core = {0, 0};
            if (prctl(PR_SET_DUMPABLE, 0L, 0L, 0L, 0L) || setrlimit(RLIMIT_CORE, &core))
                _Exit(91);
            if (cases[index].signal) {
                sigset_t only;
                sigemptyset(&only);
                sigaddset(&only, cases[index].signal);
                if (signal(cases[index].signal, SIG_DFL) == SIG_ERR
                        || sigprocmask(SIG_UNBLOCK, &only, NULL))
                    _Exit(92);
                if (kill(getpid(), cases[index].signal))
                    _Exit(93);
                _Exit(94);
            }
            _Exit(cases[index].code);
        }
        int raw_status = 0;
        pid_t observed;
        do {
            observed = waitpid(child, &raw_status, 0);
        } while (observed == -1 && errno == EINTR);
        if (observed != child)
            return 1;
        if (cases[index].signal) {
            if (!WIFSIGNALED(raw_status) || WTERMSIG(raw_status) != cases[index].signal)
                return 1;
        } else if (!WIFEXITED(raw_status) || WEXITSTATUS(raw_status) != cases[index].code) {
            return 1;
        }
        int mapped_status = observed > 0 && WIFEXITED(raw_status) ? WEXITSTATUS(raw_status) : EXIT_EXECERROR;
        if (mapped_status != (cases[index].signal ? EXIT_EXECERROR : cases[index].code))
            return 1;
        for (int cleanup = 0; cleanup < 3; cleanup++) {
            char record[MORPHEUS_WAIT_RECORD_CAPACITY], expected[MORPHEUS_WAIT_RECORD_CAPACITY];
            size_t length = morpheus_wait_record(record, sizeof(record), child, observed, raw_status,
                                                (enum morpheus_cleanup_state) cleanup);
            int size = snprintf(expected, sizeof(expected), "\nMORPHEUS_APPIMAGE_WAIT_V1 %s=%d cleanup=%s\n",
                                cases[index].signal ? "signal" : "exit",
                                cases[index].signal ? cases[index].signal : cases[index].code,
                                cleanup_names[cleanup]);
            if (size <= 0 || length != (size_t) size || strcmp(record, expected)
                    || length >= MORPHEUS_WAIT_RECORD_CAPACITY || length > PIPE_BUF)
                return 1;
            /* The observer never rewrites the caller's raw or mapped status. */
            if (mapped_status != (cases[index].signal ? EXIT_EXECERROR : cases[index].code))
                return 1;
        }
    }
    const struct { pid_t expected; pid_t observed; int raw; const char *kind; } invalid[] = {
        {42, -1, 0, "failed"}, {42, 0, 0, "other"}, {42, 43, 0, "other"},
        {0, -1, 0, "other"}, {-1, -1, 0, "other"},
        {42, 42, -1, "other"}, {42, 42, 0x10000, "other"},
        {42, 42, 0x80, "other"}, {42, 42, 0x7f, "other"}, {42, 42, 65, "other"}
    };
    for (size_t index = 0; index < sizeof(invalid) / sizeof(invalid[0]); index++) {
        char record[MORPHEUS_WAIT_RECORD_CAPACITY], expected[MORPHEUS_WAIT_RECORD_CAPACITY];
        size_t length = morpheus_wait_record(record, sizeof(record), invalid[index].expected,
                                            invalid[index].observed, invalid[index].raw, MORPHEUS_CLEANUP_PASSED);
        int size = snprintf(expected, sizeof(expected), "\nMORPHEUS_APPIMAGE_WAIT_V1 wait=%s cleanup=passed\n",
                            invalid[index].kind);
        if (size <= 0 || length != (size_t) size || strcmp(record, expected))
            return 1;
    }
    char tiny[4] = {1, 1, 1, 1};
    if (morpheus_wait_record(tiny, sizeof(tiny), 42, 42, 0, MORPHEUS_CLEANUP_PASSED) != 0
            || tiny[0] != '\0' || morpheus_wait_record(NULL, 0, 42, 42, 0, MORPHEUS_CLEANUP_PASSED) != 0
            || morpheus_wait_record(tiny, sizeof(tiny), 42, 42, 0, (enum morpheus_cleanup_state) -1) != 0)
        return 1;
    return 0;
}

static int equal_signal_sets(const sigset_t *left, const sigset_t *right) {
    for (int number = 1; number < NSIG; number++)
        if (sigismember(left, number) != sigismember(right, number))
            return 0;
    return 1;
}

static int open_fd_count(void) {
    int count = 0;
    for (int fd = 0; fd < 64; fd++)
        if (fcntl(fd, F_GETFD) != -1)
            count++;
    return count;
}

enum observer_io_case { OBS_NORMAL, OBS_CLOSED_READER, OBS_FULL, OBS_REGULAR,
                        OBS_CLOSED_STDERR, OBS_READ_ONLY, OBS_PACKET, OBS_PENDING, OBS_NO_FDS, OBS_ONE_FD };

static int observer_io_case(enum observer_io_case which) {
    int pipes[2] = {-1, -1}, saved_stderr = -1;
    FILE *regular = NULL;
    pid_t child = fork();
    if (child == -1)
        return 1;
    if (child == 0)
        _Exit(127);
    int raw_status = 0;
    pid_t observed;
    do {
        observed = waitpid(child, &raw_status, 0);
    } while (observed == -1 && errno == EINTR);
    if (observed != child || !WIFEXITED(raw_status) || WEXITSTATUS(raw_status) != 127)
        return 1;
    int original_mapped_status = WEXITSTATUS(raw_status);
    if (signal(SIGPIPE, SIG_DFL) == SIG_ERR)
        return 1;
    sigset_t only_pipe;
    sigemptyset(&only_pipe);
    sigaddset(&only_pipe, SIGPIPE);
    if (sigprocmask(which == OBS_PENDING ? SIG_BLOCK : SIG_UNBLOCK, &only_pipe, NULL))
        return 1;
    if (which == OBS_PENDING && raise(SIGPIPE))
        return 1;
    saved_stderr = dup(STDERR_FILENO);
    if (saved_stderr == -1)
        return 1;
    if (which == OBS_REGULAR) {
        regular = tmpfile();
        if (!regular || write(fileno(regular), "sentinel", 8) != 8
                || lseek(fileno(regular), 3, SEEK_SET) != 3 || dup2(fileno(regular), STDERR_FILENO) == -1)
            return 1;
    } else if (which == OBS_CLOSED_STDERR) {
        if (close(STDERR_FILENO))
            return 1;
    } else {
        if (pipe2(pipes, O_NONBLOCK | O_CLOEXEC | (which == OBS_PACKET ? O_DIRECT : 0)))
            return 1;
        if (which == OBS_FULL) {
            int capacity = fcntl(pipes[1], F_GETPIPE_SZ);
            char fill[4096];
            memset(fill, 'x', sizeof(fill));
            if (capacity <= 0 || capacity > 1024 * 1024)
                return 1;
            for (int offset = 0; offset < capacity; offset += sizeof(fill)) {
                int remaining = capacity - offset;
                size_t amount = remaining < (int) sizeof(fill) ? (size_t) remaining : sizeof(fill);
                if (write(pipes[1], fill, amount) != (ssize_t) amount)
                    return 1;
            }
        }
        /* Original stderr is blocking: only the independently reopened writer
         * may be NONBLOCK, including when its pipe is completely full. */
        int flags = fcntl(pipes[1], F_GETFL);
        if (flags == -1 || fcntl(pipes[1], F_SETFL, flags & ~O_NONBLOCK))
            return 1;
        int selected = which == OBS_READ_ONLY ? pipes[0] : pipes[1];
        if (dup2(selected, STDERR_FILENO) == -1)
            return 1;
        if (which == OBS_READ_ONLY) {
            close(pipes[0]); pipes[0] = -1;
        } else {
            close(pipes[1]); pipes[1] = -1;
        }
        if (which == OBS_CLOSED_READER) {
            close(pipes[0]); pipes[0] = -1;
        }
    }
    if (which == OBS_NO_FDS || which == OBS_ONE_FD) {
        struct rlimit files = {16, 16};
        if (setrlimit(RLIMIT_NOFILE, &files))
            return 1;
        int last = -1;
        for (int count = 0; count < 32; count++) {
            int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
            if (fd == -1) {
                if (errno != EMFILE)
                    return 1;
                break;
            }
            last = fd;
        }
        if (last == -1 || (which == OBS_ONE_FD && close(last)))
            return 1;
    }
    struct sigaction before_action, after_action;
    sigset_t before_mask, after_mask, before_pending, after_pending;
    struct stat before_info, after_info;
    int before_flags = fcntl(STDERR_FILENO, F_GETFL);
    int before_fd_flags = fcntl(STDERR_FILENO, F_GETFD);
    int had_stderr = fstat(STDERR_FILENO, &before_info) == 0;
    int before_count = open_fd_count();
    if (sigaction(SIGPIPE, NULL, &before_action) || sigprocmask(SIG_SETMASK, NULL, &before_mask)
            || sigpending(&before_pending))
        return 1;
    /* This alarm belongs only to the isolated fixture. A blocking observer
     * fails here rather than letting a full pipe consume the hosted deadline. */
    alarm(5);
    errno = EDOM;
    morpheus_observe_payload_wait(child, observed, raw_status, MORPHEUS_CLEANUP_PASSED);
    if (errno != EDOM || original_mapped_status != 127)
        return 1;
    alarm(0);
    if (open_fd_count() != before_count || fcntl(STDERR_FILENO, F_GETFL) != before_flags
            || fcntl(STDERR_FILENO, F_GETFD) != before_fd_flags
            || sigaction(SIGPIPE, NULL, &after_action) || sigprocmask(SIG_SETMASK, NULL, &after_mask)
            || sigpending(&after_pending) || before_action.sa_handler != after_action.sa_handler
            || before_action.sa_flags != after_action.sa_flags
            || !equal_signal_sets(&before_action.sa_mask, &after_action.sa_mask)
            || !equal_signal_sets(&before_mask, &after_mask)
            || !equal_signal_sets(&before_pending, &after_pending))
        return 1;
    if ((fstat(STDERR_FILENO, &after_info) == 0) != had_stderr)
        return 1;
    if (had_stderr && (before_info.st_dev != after_info.st_dev || before_info.st_ino != after_info.st_ino
            || before_info.st_mode != after_info.st_mode || before_info.st_uid != after_info.st_uid
            || before_info.st_gid != after_info.st_gid))
        return 1;
    if (which == OBS_REGULAR) {
        char data[8];
        if (lseek(STDERR_FILENO, 0, SEEK_CUR) != 3 || pread(STDERR_FILENO, data, sizeof(data), 0) != 8
                || memcmp(data, "sentinel", sizeof(data)))
            return 1;
    } else if (which == OBS_NORMAL || which == OBS_PENDING) {
        char data[MORPHEUS_WAIT_RECORD_CAPACITY];
        const char expected[] = "\nMORPHEUS_APPIMAGE_WAIT_V1 exit=127 cleanup=passed\n";
        ssize_t length = read(pipes[0], data, sizeof(data));
        if (length != (ssize_t) (sizeof(expected) - 1) || memcmp(data, expected, sizeof(expected) - 1))
            return 1;
        if (read(pipes[0], data, sizeof(data)) != -1 || errno != EAGAIN)
            return 1;
    } else if (which == OBS_FULL) {
        char data[4096];
        int total = 0;
        ssize_t length;
        while ((length = read(pipes[0], data, sizeof(data))) > 0) {
            for (ssize_t index = 0; index < length; index++)
                if (data[index] != 'x')
                    return 1;
            total += (int) length;
            if (total > 1024 * 1024)
                return 1;
        }
        if (length != -1 || errno != EAGAIN || total != fcntl(STDERR_FILENO, F_GETPIPE_SZ))
            return 1;
    } else if (which == OBS_READ_ONLY || which == OBS_PACKET) {
        char data;
        if (read(which == OBS_READ_ONLY ? STDERR_FILENO : pipes[0], &data, 1) != -1 || errno != EAGAIN)
            return 1;
    }
    if (dup2(saved_stderr, STDERR_FILENO) == -1 || close(saved_stderr))
        return 1;
    if (pipes[0] != -1) close(pipes[0]);
    if (pipes[1] != -1) close(pipes[1]);
    if (regular && fclose(regular))
        return 1;
    return 0;
}

static int wait_writer_cases(void) {
    for (int which = OBS_NORMAL; which <= OBS_ONE_FD; which++) {
        pid_t child = fork();
        if (child == -1)
            return 1;
        if (child == 0)
            _Exit(observer_io_case((enum observer_io_case) which));
        int status = 0;
        pid_t observed;
        do {
            observed = waitpid(child, &status, 0);
        } while (observed == -1 && errno == EINTR);
        if (observed != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
            return 1;
    }
    return 0;
}

int main(void) {
    const struct fixture_case cases[] = {
        {HOME, 16, 0, 0}, {HOME, PATH_MAX - 6, 0, 0},
        {HOME, PATH_MAX - 5, EXIT_FAILURE, 0},
        {CONFIG, PATH_MAX - 8, 0, 0}, {CONFIG, PATH_MAX - 7, EXIT_FAILURE, 0},
        {HOME, PATH_MAX, EXIT_FAILURE, 0}, {HOME, PATH_MAX + 1, EXIT_FAILURE, 0},
        {CONFIG, PATH_MAX, EXIT_FAILURE, 0}, {HOME, -1, EXIT_FAILURE, 0},
        {HELP, PATH_MAX - 1, 0, 0}, {HELP, PATH_MAX, EXIT_EXECERROR, 0},
        {HELP, PATH_MAX + 1, EXIT_EXECERROR, 0}, {HELP, -1, EXIT_EXECERROR, 0},
        {STARTUP, PATH_MAX - 1, 0, 0}, {STARTUP, PATH_MAX, EXIT_EXECERROR, 0},
        {STARTUP, PATH_MAX + 1, EXIT_EXECERROR, 0}, {STARTUP, -1, EXIT_EXECERROR, 0},
        {IMPLICIT, 16, 0, 2}, {IMPLICIT, PATH_MAX - 8, 0, 2},
        {IMPLICIT, PATH_MAX - 7, 0, 1}, {IMPLICIT, PATH_MAX - 6, 0, 1},
        {IMPLICIT, PATH_MAX - 5, 0, 0}, {IMPLICIT, PATH_MAX - 1, 0, 0},
    };
    for (size_t index = 0; index < sizeof(cases) / sizeof(cases[0]); index++) {
        fixture_mode = cases[index].mode;
        fixture_length = cases[index].length;
        readlink_calls = mkdir_calls = access_calls = 0;
        expected_readlink = fixture_mode != IMPLICIT;
        expected_mkdir = (fixture_mode == HOME || fixture_mode == CONFIG) && cases[index].status == 0;
        expected_access = cases[index].access;
        pid_t child = fork();
        if (child == -1)
            return 1;
        if (child == 0) {
            if (close(STDOUT_FILENO))
                _Exit(96);
            if (atexit(verify_calls))
                _Exit(94);
            if (fixture_mode == IMPLICIT) {
                char base[PATH_MAX];
                memset(base, 'x', (size_t) fixture_length);
                base[fixture_length] = '\0';
                set_portable_home_and_config(base);
                exit(0);
            }
            if (fixture_mode == HOME || fixture_mode == CONFIG) {
                portable_option(fixture_mode == HOME ? "appimage-portable-home" : "appimage-portable-config",
                                "/synthetic/runtime", fixture_mode == HOME ? "home" : "config");
            } else {
                char *argv[] = {"runtime-path-regression", "--appimage-help", NULL};
                morpheus_runtime_main(fixture_mode == HELP ? 2 : 1, argv);
            }
            _Exit(95);
        }
        int status;
        pid_t observed;
        do {
            observed = waitpid(child, &status, 0);
        } while (observed == -1 && errno == EINTR);
        if (observed != child || !WIFEXITED(status) || WEXITSTATUS(status) != cases[index].status)
            return 1;
    }
    if (wait_observation_cases() || wait_writer_cases())
        return 1;
    puts("Morpheus runtime path regression passed.");
    return 0;
}
