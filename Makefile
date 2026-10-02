.DEFAULT_GOAL := all

ARCH ?= riscv64
CPUS ?= 4
MEMORY ?= 1G
MODE ?= debug
FEATURES ?=
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

.PHONY: all kernel-rv kernel-la build run debug test clean

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

clean:
	cd os && cargo clean
	rm -rf build
	rm -f kernel-rv kernel-la
