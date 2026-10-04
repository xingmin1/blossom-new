.DEFAULT_GOAL := all

ARCH ?= riscv64
CPUS ?= 4
MEMORY ?= 1G
MODE ?= debug
PANIC ?= unwind
KTEST_ARGS ?=
FEATURES ?=
FAULT ?= irq
OUTPUT ?= build/$(ARCH)/kernel.elf

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
QEMU := qemu-system-riscv64
BIOS := -bios default
else ifeq ($(ARCH),loongarch64)
TARGET := loongarch64-unknown-none
QEMU := qemu-system-loongarch64
BIOS :=
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

QEMU_ARGS := -machine virt -accel tcg $(BIOS) -m $(MEMORY) -smp $(CPUS) -nographic -monitor none -no-reboot -nic none
CARGO_ARGS := --locked --target $(TARGET) $(CARGO_MODE) $(if $(FEATURES),--features $(FEATURES))

.PHONY: all kernel-rv kernel-la build run debug test test-memory test-execution test-execution-fault clean

all: kernel-rv kernel-la

kernel-rv:
	$(MAKE) build ARCH=riscv64 FEATURES= OUTPUT=build/riscv64/kernel.elf
	cp build/riscv64/kernel.elf $@

kernel-la:
	$(MAKE) build ARCH=loongarch64 FEATURES= OUTPUT=build/loongarch64/kernel.elf
	cp build/loongarch64/kernel.elf $@

build:
	cd os && cargo build $(CARGO_ARGS)
	mkdir -p $(dir $(OUTPUT))
	cp os/target/$(TARGET)/$(MODE)/os $(OUTPUT)

run: build
	$(QEMU) $(QEMU_ARGS) -kernel $(OUTPUT)

debug: build
	$(QEMU) $(QEMU_ARGS) -kernel $(OUTPUT) -S -gdb tcp:127.0.0.1:1234

test:
	$(MAKE) build FEATURES=boot-tests OUTPUT=build/$(ARCH)/kernel-boot-tests.elf
	python3 scripts/test-boot.py --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --kernel build/$(ARCH)/kernel-boot-tests.elf
	$(MAKE) build FEATURES=fault-test OUTPUT=build/$(ARCH)/kernel-fault-test.elf
	python3 scripts/test-boot.py --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --kernel build/$(ARCH)/kernel-fault-test.elf --fault illegal
ifeq ($(ARCH),loongarch64)
	$(MAKE) build FEATURES=refill-test OUTPUT=build/$(ARCH)/kernel-refill-test.elf
	python3 scripts/test-boot.py --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --kernel build/$(ARCH)/kernel-refill-test.elf --fault refill
endif

test-memory:
	$(MAKE) build ARCH=$(ARCH) CPUS=$(CPUS) MEMORY="$(MEMORY)" MODE=$(MODE) FEATURES=memory-tests OUTPUT=build/$(ARCH)/kernel-memory-tests.elf
	python3 scripts/test-memory.py --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --kernel build/$(ARCH)/kernel-memory-tests.elf

test-execution:
	$(MAKE) build ARCH=$(ARCH) CPUS=$(CPUS) MEMORY="$(MEMORY)" MODE=$(MODE) FEATURES=execution-tests OUTPUT=build/$(ARCH)/kernel-execution-tests.elf
	python3 scripts/test-execution.py --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --kernel build/$(ARCH)/kernel-execution-tests.elf

test-execution-fault:
	@test "$(FAULT)" = irq -o "$(FAULT)" = preempt -o "$(FAULT)" = printer-oom -o "$(FAULT)" = timekeeper || { echo "FAULT must be irq, preempt, printer-oom, or timekeeper" >&2; exit 2; }
	$(MAKE) build ARCH=$(ARCH) CPUS=$(CPUS) MEMORY="$(MEMORY)" MODE=$(MODE) FEATURES=execution-$(FAULT)-fault OUTPUT=build/$(ARCH)/kernel-execution-$(FAULT)-fault.elf
	python3 scripts/test-execution.py --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --kernel build/$(ARCH)/kernel-execution-$(FAULT)-fault.elf --fault $(FAULT)

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
	python3 tools/ktest.py list --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --panic $(PANIC) $(KTEST_ARGS)

ktest:
	python3 tools/ktest.py run --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --panic $(PANIC) $(KTEST_ARGS)

clean:
	cd os && cargo clean
	rm -rf build
	rm -f kernel-rv kernel-la
