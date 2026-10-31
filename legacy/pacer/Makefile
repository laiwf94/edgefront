# pacer build. `make` builds release binaries, `make check` runs the gate.
# Variants live in build/<variant>: release, asan (ASan+UBSan), tsan, cov.

CC       := clang
VARIANT  ?= release
UNAME    := $(shell uname -s)
# Native builds on other systems get their own tree, so they never mix
# objects with the Linux container's when the checkout is shared.
ifeq ($(UNAME),Linux)
  BUILD  := build/$(VARIANT)
else
  BUILD  := build/$(UNAME)/$(VARIANT)
endif
JOBS     ?= $(shell nproc 2>/dev/null || sysctl -n hw.ncpu)

CPPFLAGS := -Iinclude -Isrc -D_GNU_SOURCE
WARN     := -Wall -Wextra -Werror -Wpedantic -Wshadow -Wconversion -Wstrict-prototypes \
            -Wmissing-prototypes -Wformat=2 -Wundef -Wcast-qual -Wvla
CFLAGS   := -std=c17 $(WARN) -g -pthread -MMD -MP
LDFLAGS  := -pthread

ifeq ($(VARIANT),release)
  CFLAGS += -O2 -D_FORTIFY_SOURCE=2 -fstack-protector-strong
  # STATIC=1 links libc statically for the scratch Docker image. pacerd
  # does no name lookups, so static glibc has no NSS caveats here.
  ifeq ($(STATIC),1)
    LDFLAGS += -static
  endif
else ifeq ($(VARIANT),asan)
  SAN := -fsanitize=address,undefined -fno-sanitize-recover=all -fno-omit-frame-pointer
  CFLAGS += -O1 $(SAN)
  LDFLAGS += $(SAN)
else ifeq ($(VARIANT),tsan)
  CFLAGS += -O1 -fsanitize=thread
  LDFLAGS += -fsanitize=thread
else ifeq ($(VARIANT),cov)
  CFLAGS += -O0 -fprofile-instr-generate -fcoverage-mapping
  LDFLAGS += -fprofile-instr-generate
else
  $(error unknown VARIANT $(VARIANT))
endif

# pacerd and pacerctl need Linux (epoll, signalfd, eventfd, SOCK_CLOEXEC);
# elsewhere only the library, the portable parts of the daemon and the unit
# tests are built.
LINUX_SRC := src/pacerd/daemon.c src/pacerd/worker.c src/pacerd/proxy.c src/pacerd/admin.c
LIB_SRC   := $(wildcard src/lib/*.c)
PD_ALL    := $(filter-out src/pacerd/main.c,$(wildcard src/pacerd/*.c))
ifeq ($(UNAME),Linux)
  PD_SRC  := $(PD_ALL)
  BINS    := $(BUILD)/pacerd $(BUILD)/pacerctl
else
  PD_SRC  := $(filter-out $(LINUX_SRC),$(PD_ALL))
  BINS    :=
endif
TEST_SRC  := $(wildcard tests/test_*.c tests/model_*.c) tests/test_main.c
IT_SRC    := $(wildcard tests/it_*.c) tests/harness.c tests/upstream.c tests/test_main.c
LIB_OBJ   := $(LIB_SRC:%.c=$(BUILD)/%.o)
PD_OBJ    := $(PD_SRC:%.c=$(BUILD)/%.o)
TEST_OBJ  := $(sort $(TEST_SRC:%.c=$(BUILD)/%.o))
IT_OBJ    := $(sort $(IT_SRC:%.c=$(BUILD)/%.o))

FUZZ_SRC  := $(wildcard fuzz/fuzz_*.c)
BENCH_SRC := bench/bench_limiter.c bench/bench_proxy.c
ALL_C     := $(LIB_SRC) $(PD_ALL) src/pacerd/main.c src/pacerctl/pacerctl.c \
             $(sort $(TEST_SRC) $(IT_SRC)) $(FUZZ_SRC) $(BENCH_SRC)
ALL_SRC   := $(ALL_C) $(wildcard include/*.h src/*/*.h tests/*.h)

.PHONY: all lib test run-tests check format format-check tidy coverage clean require-linux \
        fuzz fuzz-smoke bench bench-build

all: lib $(BINS)

lib: $(BUILD)/libpacer.a

$(BUILD)/%.o: %.c
	@mkdir -p $(dir $@)
	$(CC) $(CPPFLAGS) $(CFLAGS) -c $< -o $@

$(BUILD)/libpacer.a: $(LIB_OBJ)
	@rm -f $@
	ar rcs $@ $^

$(BUILD)/libpacerd.a: $(PD_OBJ)
	@rm -f $@
	ar rcs $@ $^

$(BUILD)/unit: $(TEST_OBJ) $(BUILD)/libpacerd.a $(BUILD)/libpacer.a
	$(CC) $(LDFLAGS) -o $@ $^

$(BUILD)/pacerd: $(BUILD)/src/pacerd/main.o $(BUILD)/libpacerd.a $(BUILD)/libpacer.a
	$(CC) $(LDFLAGS) -o $@ $^

$(BUILD)/pacerctl: $(BUILD)/src/pacerctl/pacerctl.o $(BUILD)/libpacerd.a $(BUILD)/libpacer.a
	$(CC) $(LDFLAGS) -o $@ $^

$(BUILD)/itest: $(IT_OBJ)
	$(CC) $(LDFLAGS) -o $@ $^

# LeakSanitizer is part of ASan on Linux only.
ifeq ($(UNAME),Linux)
  LEAKS := 1
else
  LEAKS := 0
endif
TEST_ENV := ASAN_OPTIONS=detect_leaks=$(LEAKS):abort_on_error=1 \
            UBSAN_OPTIONS=print_stacktrace=1:halt_on_error=1 \
            TSAN_OPTIONS=halt_on_error=1:second_deadlock_stack=1 \
            LLVM_PROFILE_FILE=$(abspath $(BUILD))/prof/%p-%m.profraw \
            PACERD_BIN=$(BUILD)/pacerd PACERCTL_BIN=$(BUILD)/pacerctl

$(BUILD)/bench_limiter: $(BUILD)/bench/bench_limiter.o $(BUILD)/libpacer.a
	$(CC) $(LDFLAGS) -o $@ $^

$(BUILD)/bench_proxy: $(BUILD)/bench/bench_proxy.o $(BUILD)/tests/harness.o \
                      $(BUILD)/tests/upstream.o
	$(CC) $(LDFLAGS) -o $@ $^

bench-build:
	$(MAKE) VARIANT=release build/release/bench_limiter build/release/bench_proxy build/release/pacerd

# Benchmarks run on release builds; numbers depend on the machine.
bench: bench-build
	build/release/bench_limiter
	PACERD_BIN=build/release/pacerd build/release/bench_proxy

ifeq ($(UNAME),Linux)
run-tests: $(BUILD)/unit $(BUILD)/pacerd $(BUILD)/pacerctl $(BUILD)/itest
	$(TEST_ENV) $(BUILD)/unit
	$(TEST_ENV) $(BUILD)/itest
else
run-tests: $(BUILD)/unit
	$(TEST_ENV) $(BUILD)/unit
endif

test:
	$(MAKE) VARIANT=asan run-tests

format:
	clang-format -i $(ALL_SRC)

format-check:
	clang-format --dry-run --Werror $(ALL_SRC)

tidy:
	printf '%s\n' $(ALL_C) | xargs -P $(JOBS) -I{} clang-tidy --quiet {} -- $(CPPFLAGS) -std=c17

coverage:
	rm -rf build/cov/prof
	$(MAKE) VARIANT=cov run-tests
	scripts/coverage.sh build/cov 90 build/cov/unit build/cov/pacerd build/cov/pacerctl

# libFuzzer targets over the parsers that read outside input. They link the
# portable sources directly, built with the fuzzer and sanitizers.
FUZZ_TIME  ?= 10
FUZZ_FLAGS := -std=c17 -g -O1 -fsanitize=fuzzer,address,undefined -fno-sanitize-recover=all
FUZZ_DEPS  := $(LIB_SRC) src/pacerd/config.c src/pacerd/log.c src/pacerd/util.c \
              src/pacerd/admin_proto.c
FUZZ_BINS  := $(FUZZ_SRC:fuzz/%.c=build/fuzz/%)

build/fuzz/fuzz_%: fuzz/fuzz_%.c $(FUZZ_DEPS)
	@mkdir -p $(dir $@)
	$(CC) $(CPPFLAGS) $(FUZZ_FLAGS) $(WARN) -o $@ $^

fuzz: $(FUZZ_BINS)

# Replay the seed corpus, then fuzz each target for FUZZ_TIME seconds in a
# scratch copy of its corpus so new inputs never land in the tree.
fuzz-smoke: fuzz
	set -e; for t in config admin; do \
	  rm -rf build/fuzz/corpus-$$t && cp -r fuzz/corpus/$$t build/fuzz/corpus-$$t; \
	  dict=""; [ -f fuzz/$$t.dict ] && dict="-dict=fuzz/$$t.dict"; \
	  build/fuzz/fuzz_$$t -max_total_time=$(FUZZ_TIME) -max_len=8192 -print_final_stats=1 \
	    $$dict build/fuzz/corpus-$$t 2>&1 | tail -n 12; \
	done

require-linux:
ifneq ($(UNAME),Linux)
	@echo "make check needs Linux (epoll, signalfd, sanitizers). Run: scripts/dev.sh make check" >&2
	@exit 2
endif

check: require-linux
	$(MAKE) format-check
	$(MAKE) tidy
	$(MAKE) -j$(JOBS) VARIANT=release all
	$(MAKE) -j$(JOBS) bench-build
	$(MAKE) -j$(JOBS) VARIANT=asan run-tests
	$(MAKE) -j$(JOBS) VARIANT=tsan run-tests
	$(MAKE) -j$(JOBS) coverage
	$(MAKE) fuzz-smoke FUZZ_TIME=5
	@echo "check: all gates passed"

clean:
	rm -rf build

-include $(BUILD)/bench/bench_limiter.d $(BUILD)/bench/bench_proxy.d
-include $(LIB_OBJ:.o=.d) $(PD_OBJ:.o=.d) $(TEST_OBJ:.o=.d) $(IT_OBJ:.o=.d) $(BUILD)/src/pacerd/main.d $(BUILD)/src/pacerctl/pacerctl.d
