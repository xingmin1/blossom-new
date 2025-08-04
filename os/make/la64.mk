# Target configuration
TARGET := loongarch64-unknown-none
MODE := release
KERNEL_ELF := target/$(TARGET)/$(MODE)/os
KERNEL_BIN := $(KERNEL_ELF).bin
DISASM_TMP := target/$(TARGET)/$(MODE)/asm
BLK_MODE := virt_pci
FS_MODE ?= ext4
ROOTFS_IMG_NAME := rootfs-la.img
ROOTFS_IMG_DIR := ../fs-img-dir
CORE_NUM := 1
LOG := off
KERNEL_LA := ../kernel-la
SDCARD_LA := ../sdcard-la.img

# BOARD
BOARD ?= laqemu

# SBI config (can be simplified later)
SBI ?= opensbi-1.0
ifeq ($(BOARD), laqemu)
	ifeq ($(SBI), rustsbi)
		BOOTLOADER := ../bootloader/$(SBI)-$(BOARD).bin
	else ifeq ($(SBI), default)
		BOOTLOADER := default
	else
		BOOTLOADER := ../bootloader/fw_payload.bin
	endif
else ifeq ($(BOARD), vf2)
	BOOTLOADER := ../bootloader/rustsbi-$(BOARD).bin
endif

# Logging config
ifndef LOG
	LOG_OPTION := "log_off"
else
	LOG_OPTION := "log_${LOG}"
endif

# KERNEL entry address
KERNEL_ENTRY_PA := 0x9000000090000000

# Binutils
OBJDUMP := rust-objdump --arch-name=loongarch64
OBJCOPY := rust-objcopy --binary-architecture=loongarch64

# Applications
APPS := ../user/src/bin/*

# FS image
ifeq ($(BOARD), vf2)
	ROOTFS_IMG := /dev/sdc
else
	ROOTFS_IMG := ${ROOTFS_IMG_DIR}/${ROOTFS_IMG_NAME}
endif

# Build rules
all: fs-img build

mv:
	cp -f $(KERNEL_ELF) $(KERNEL_LA)

build: env $(KERNEL_BIN) mv

env:
	(rustup target list | grep "$(TARGET) (installed)") || rustup target add $(TARGET)
	(rustup target list | grep "loongarch64-unknown-none (installed)") || rustup target add loongarch64-unknown-none
	rustup component add rust-src
	rustup component add llvm-tools-preview

# build all user programs
user:
	@cd ../user && make rust-user BOARD=$(BOARD) MODE=$(MODE)

$(KERNEL_BIN): kernel
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

fs-img: user
	./buildfs.sh "$(ROOTFS_IMG)" "$(BOARD)" $(MODE) $(FS_MODE)

kernel:
	@echo Platform: $(BOARD)
	@echo BLK_MODE: $(BLK_MODE)
ifeq ($(MODE), debug)
	@LOG=$(LOG) cargo build --features "board_$(BOARD) $(LOG_OPTION) block_$(BLK_MODE) oom_handler" --no-default-features --target loongarch64-unknown-none
else
	@LOG=$(LOG) cargo build --release --features "board_$(BOARD) $(LOG_OPTION) block_$(BLK_MODE) oom_handler" --no-default-features --target loongarch64-unknown-none
endif

clean:
	@cargo clean
	@rm -rf $(KERNEL_LA)

run: build
ifeq ($(BOARD), laqemu)
	@qemu-system-loongarch64 \
		-machine virt \
		-nographic \
		-bios $(BOOTLOADER) \
		-device loader,file=$(KERNEL_BIN),addr=$(KERNEL_ENTRY_PA) \
		-drive if=none,file=$(ROOTFS_IMG),format=raw,id=x0 \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-m 1024 \
		-smp threads=$(CORE_NUM)
endif

monitor:
	loongarch64-unknown-elf-gdb -ex 'file target/loongarch64-unknown-none/debug/os' -ex 'set arch loongarch64' -ex 'target remote localhost:1234'

gdb:
	@qemu-system-loongarch64 \
		-machine virt \
		-nographic \
		-bios $(BOOTLOADER) \
		-device loader,file=target/loongarch64-unknown-none/debug/os,addr=$(KERNEL_ENTRY_PA) \
		-drive file=$(ROOTFS_IMG),if=none,format=raw,id=x0 \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-m 1024 \
		-smp threads=$(CORE_NUM) -S -s | tee qemu.log

runsimple:
	@qemu-system-loongarch64 \
		-machine virt \
		-nographic \
		-bios $(BOOTLOADER) \
		-device loader,file=$(KERNEL_ELF),addr=$(KERNEL_ENTRY_PA) \
		-drive file=$(ROOTFS_IMG),if=none,format=raw,id=x0 \
		-m 1024 \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-smp threads=$(CORE_NUM)

comp:
	@qemu-system-loongarch64 \
		-machine virt \
		-kernel $(KERNEL_LA) \
		-m 1024 \
		-nographic \
		-smp 1 \
		-drive file=$(SDCARD_LA),if=none,format=raw,id=x0 \
		-device virtio-blk-pci,drive=x0\
		-no-reboot \
		-rtc base=utc

comp-gdb:
	@qemu-system-loongarch64 \
		-machine virt \
		-kernel $(KERNEL_LA) \
		-m 1024 \
		-nographic \
		-smp 1 \
		-drive file=$(SDCARD_LA),if=none,format=raw,id=x0 \
		-device virtio-blk-pci,drive=x0 \
		-no-reboot \
		-rtc base=utc \
		-S \
		-s

.PHONY: all build kernel fs-img user clean run gdb comp comp-gdb