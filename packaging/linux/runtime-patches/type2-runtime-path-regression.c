/* SPDX-License-Identifier: AGPL-3.0-only
 * Exercise the exact patched runtime source. All path probes are intercepted;
 * no directory, mount, payload extraction or application execution is allowed.
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
    puts("Morpheus runtime path regression passed.");
    return 0;
}
