.DEFAULT_GOAL := all

ARCH ?= riscv64
CPUS ?= 4
MEMORY ?= $(if $(filter riscv64,$(ARCH)),512M,768M)
MODE ?= debug
PANIC ?= unwind
KTEST_ARGS ?=
FEATURES ?=
OUTPUT ?= build/$(ARCH)/kernel.elf
TEST ?=
ROOT ?= /dev/vda
EXTRA_ROOT ?=
IDENTITY ?= root
IMAGE ?= build/$(if $(filter extra,$(IDENTITY)),extra,system)-$(ARCH).img
STAGING ?= build/system-$(ARCH)-$(IDENTITY)
SOURCE ?=
EXTRA_IMAGE ?=
SCRATCH_IMAGE ?=
WORK_DIR ?= build/run-$(ARCH)
RESUME ?= 0
LLVM_MC ?= llvm-mc
USER_LD ?= rust-lld
USER_OUTPUTS := $(addprefix build/user-$(ARCH)/user-,smoke io clone signal futex)

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
QEMU := qemu-system-riscv64
USER_TRIPLE := riscv64-unknown-linux-gnu
USER_MATTR := +m,+a,+f,+d,-c,-v
USER_EMULATION := elf64lriscv
else ifeq ($(ARCH),loongarch64)
TARGET := loongarch64-unknown-none
QEMU := qemu-system-loongarch64
USER_TRIPLE := loongarch64-unknown-linux-gnu
USER_MATTR := +f,+d,-lsx,-lasx
USER_EMULATION := elf64loongarch
else
$(error ARCH 只支持 riscv64 或 loongarch64)
endif

ifneq ($(words $(CPUS)),1)
$(error CPUS 必须是一个核数)
endif
ifeq ($(filter $(CPUS),1 2 4),)
$(error 当前阶段 CPUS 只支持 1、2、4)
endif

ifeq ($(MODE),release)
CARGO_MODE := --release
else ifneq ($(MODE),debug)
$(error MODE 只支持 debug 或 release)
endif

CARGO_ARGS := --locked --target $(TARGET) $(CARGO_MODE) $(if $(FEATURES),--features $(FEATURES))

.PHONY: all kernel-rv kernel-la build image run debug clean

all: kernel-rv kernel-la

kernel-rv:
	$(MAKE) build ARCH=riscv64 FEATURES= OUTPUT=build/riscv64/kernel.elf && cp build/riscv64/kernel.elf $@

kernel-la:
	$(MAKE) build ARCH=loongarch64 FEATURES= OUTPUT=build/loongarch64/kernel.elf && cp build/loongarch64/kernel.elf $@

build:
	cd os && cargo build $(CARGO_ARGS)
	mkdir -p $(dir $(OUTPUT))
	cp os/target/$(TARGET)/$(MODE)/os "$(OUTPUT)"

image:
	python3 tools/mkext4.py --arch $(ARCH) --output "$(IMAGE)" --identity $(IDENTITY) --profile seed4k --size-mib 256 $(if $(SOURCE),--source "$(SOURCE)",--staging "$(STAGING)")

run debug: build
	@case "$(RESUME)" in 0|1) ;; *) echo "RESUME 只支持 0 或 1"; exit 1;; esac
	python3 tools/run.py --arch $(ARCH) --kernel "$(OUTPUT)" --cpus $(CPUS) --memory "$(MEMORY)" --qemu "$(QEMU)" --image "$(IMAGE)" --root "$(ROOT)" --work-dir "$(WORK_DIR)" $(if $(filter 1,$(RESUME)),--resume) $(if $(EXTRA_IMAGE),--extra-image "$(EXTRA_IMAGE)") $(if $(SCRATCH_IMAGE),--scratch-image "$(SCRATCH_IMAGE)") $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(filter debug,$@),--debug)


ifeq ($(PANIC),unwind)
KTEST_PROFILE := $(if $(filter release,$(MODE)),ktest-release,ktest-dev)
else ifeq ($(PANIC),abort)
KTEST_PROFILE := $(if $(filter release,$(MODE)),ktest-abort-release,ktest-abort-dev)
else
$(error PANIC 只支持 unwind 或 abort)
endif

.PHONY: ktest-build ktest-list ktest
ktest-build:
	cd os && CARGO_TARGET_DIR=../build/ktest-target RUSTFLAGS="-Cpanic=$(PANIC) -Cforce-frame-pointers=yes -Cforce-unwind-tables=yes" cargo build --locked --target $(TARGET) --profile $(KTEST_PROFILE) --features ktest -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem
	mkdir -p "$(dir $(OUTPUT))"
	cp "build/ktest-target/$(TARGET)/$(KTEST_PROFILE)/os" "$(OUTPUT)"

ktest-list:
	python3 tools/ktest.py list --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --panic $(PANIC) --root "$(ROOT)" $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(TEST),--glob "$(TEST)") $(KTEST_ARGS)

ktest:
	python3 tools/ktest.py run --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --panic $(PANIC) --root "$(ROOT)" $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(TEST),--glob "$(TEST)") $(KTEST_ARGS)

.PHONY: user-tests
user-tests: $(USER_OUTPUTS)

build/user-$(ARCH)/user-%: tests/user/%-$(ARCH).S tests/user/smoke.ld
	mkdir -p "$(dir $@)"
	$(LLVM_MC) --triple=$(USER_TRIPLE) --mattr=$(USER_MATTR) --filetype=obj "$<" -o "$@.o"
	$(USER_LD) -flavor gnu -static --no-dynamic-linker --no-relax --build-id=none -z max-page-size=4096 -T tests/user/smoke.ld -m $(USER_EMULATION) "$@.o" -o "$@"

clean:
	cd os && cargo clean
	rm -rf build
	rm -f kernel-rv kernel-la
