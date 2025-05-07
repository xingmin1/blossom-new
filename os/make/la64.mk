# run: 清除编译结果，重新编译，运行
# all: 直接编译，并把.bin内核拷贝到根目录（适配大赛要求）
# gdb: 只运行gdb（需要先通过make run来编译）
# clean: 清除编译结果
SUDO := $(shell if [ "$$(whoami)" = "root" ]; then echo ""; else echo "sudo"; fi)

ARCH := loongarch64

TARGET := loongarch64-unknown-linux-gnu

MODE := debug
FS_MODE ?= ext4

CORE_NUM ?= 1
BIOS ?= ../util/qemu-2k1000/gz/u-boot-with-spl.bin
QEMU ?= ../util/qemu-2k1000/tmp/qemu/bin/qemu-system-loongarch64

KERNEL_ELF = target/$(TARGET)/$(MODE)/os
KERNEL_BIN = $(KERNEL_ELF).bin
KERNEL_UIMG = $(KERNEL_ELF).ui

BOARD ?= laqemu
LDBOARD = la2k1000

COMP ?= yes
LOG := error

# 大写K转小写
ifeq ($(BOARD), 2K1000)
	BOARD = 2k1000
else ifeq ($(BOARD), K210)
	BOARD = k210
endif

# 块设备类型
BLK_MODE ?= mem

# Binutils
OBJCOPY := loongarch64-linux-gnu-objcopy
OBJDUMP := loongarch64-linux-gnu-objdump
READELF := loongarch64-linux-gnu-readelf

ifndef LOG
	LOG_OPTION := "log_off"
else
	LOG_OPTION := "log_${LOG}"
endif

ifeq ($(MODE), debug)
	LA_2k1000_DISABLE_EH_FRAME := -D EH_ENABLED
endif

IMG_DIR := ../fs-img-dir
IMG_NAME = rootfs-ubifs-ze.img
IMG := ${IMG_DIR}/$(IMG_NAME)
IMG_LN = $(shell readlink -f $(IMG_DIR))/$(IMG_NAME)

QEMU_2k1000_DIR=../util/qemu-2k1000/gz
QEMU_2k1000=$(QEMU_2k1000_DIR)/runqemu2k1000
U_IMG=$(IMG_DIR)/uImage

LA_DEBUGGER_SERIAL_PORT = $$(python3 -m serial.tools.list_ports 1A86:7523 -q | head -n 1)
LA_DEBUGGER_PORT_FREQ = $(LA_DEBUGGER_SERIAL_PORT) 115200
LA_2k1000_SERIAL_PORT = $$(python3 -m serial.tools.list_ports 067B:2303 -q | head -n 1)
LA_2k1000_PORT_FREQ = $(LA_2k1000_SERIAL_PORT) 115200
MINITERM_START_CMD=python3 -m serial.tools.miniterm --dtr 0 --rts 0 --filter direct

LA_ENTRY_POINT = 0x9000000090000000
LA_LOAD_ADDR = 0x9000000090000000

runsimple: env update-usr
	qemu-system-loongarch64 \
	-machine virt \
	-kernel ${KERNEL_ELF} \
	-m 1024 \
	-nographic \
	-smp threads=${CORE_NUM} \
	-no-reboot \
	-device virtio-net-pci,netdev=net0 \
	-netdev user,id=net0,hostfwd=tcp::5555-:5555,hostfwd=udp::5555-:5555 \
	-rtc base=utc

run: do-run

comp:
	qemu-system-loongarch64 \
	-kernel ../kernel-la \
	-m 1G \
	-nographic \
	-smp 1 \
	-drive file=../sdcard-la.img,if=none,format=raw,id=x0 \
	-device virtio-blk-pci,drive=x0 \
	-no-reboot \
	-device virtio-net-pci,netdev=net0 \
	-netdev user,id=net0 \
	-rtc base=utc

comp-1:
	qemu-system-loongarch64 \
	-kernel {os_file} \
	-m {mem} \
	-nographic \
	-smp {smp} \
	-drive file={fs},if=none,format=raw,id=x0  \
	-device virtio-blk-pci,drive=x0,bus=virtio-mmio-bus.0 \
	-no-reboot \
	-device virtio-net-pci,netdev=net0 \
	-netdev user,id=net0,hostfwd=tcp::5555-:5555,hostfwd=udp::5555-:5555 \
	-rtc base=utc \
	-drive file=disk-la.img,if=none,format=raw,id=x1 \
	-device virtio-blk-pci,drive=x1,bus=virtio-mmio-bus.1

# 更新用户态程序
update-usr: fs-img

# 编译用户态程序
user: env
	@cd ../user && make rust-user BOARD=$(BOARD) MODE=$(MODE)

# 生成根文件系统镜像
fs-img: user
ifeq ($(BOARD),laqemu)
	@$(SUDO) rm -rf $(IMG)
	./buildfs.sh "$(IMG)" "laqemu" $(MODE) $(FS_MODE)
else
	./buildfs.sh "$(IMG)" 2k1000 $(MODE) $(FS_MODE)
endif

# 仅更新内核并运行
run-inner: uimage do-run

# 将内核转换为二进制文件
$(KERNEL_BIN): kernel
	@$(OBJCOPY) $(KERNEL_ELF) $@ --strip-all -O binary &
	@$(OBJDUMP) $(KERNEL_ELF) -SC > target/$(TARGET)/$(MODE)/asm_all.txt
	@$(READELF) -ash $(KERNEL_ELF) > target/$(TARGET)/$(MODE)/sec.txt &

# 编译内核
kernel:
	@echo Platform: $(BOARD)
    ifeq ($(MODE), debug)
		@LOG=$(LOG) cargo build --no-default-features --features "comp board_$(BOARD) block_$(BLK_MODE) $(LOG_OPTION)" --target $(TARGET)
    else
		@LOG=$(LOG) cargo build --no-default-features --release --features "comp board_$(BOARD) block_$(BLK_MODE) $(LOG_OPTION)"  --target $(TARGET)
    endif

# 更新内核
uimage: env $(KERNEL_BIN)
	../util/mkimage -A loongarch -O linux -T kernel -C none -a $(LA_LOAD_ADDR) -e $(LA_ENTRY_POINT) -n NPUcore+ -d $(KERNEL_BIN) $(KERNEL_UIMG)
	-@$(SUDO) rm $(U_IMG)
	@$(SUDO) cp -f $$(pwd)/target/$(TARGET)/$(MODE)/os.ui $(U_IMG)

do-run:
ifeq ($(BOARD), laqemu)
# 将镜像链接到指定目录
	-ln -sf $(IMG_LN) $(QEMU_2k1000_DIR)/$(IMG_NAME)
	@echo "========WARNING!========"
	@echo "下一个命令是修改后的runqemu2k1000脚本，其中任何潜在的和隐式的“当前工作目录”已被生成的脚本存储路径所替换。"
	@./run_script $(QEMU_2k1000)
else ifeq ($(BOARD), 2k1000)
	@./run_script $(MINITERM_START_CMD) $(LA_2k1000_PORT_FREQ)
endif

# 生成根文件系统镜像并编译内核
all: fs-img uimage mv

mv:
	cp -f $(KERNEL_ELF) ../kernel-la

gdb:
ifeq ($(BOARD),laqemu)
	./run_script $(QEMU_2k1000) "-S"
else ifeq ($(BOARD), 2k1000)
	@./la_gdbserver minicom -D $(LA_DEBUGGER_PORT_FREQ)
endif

env: # 切换工具链
	-(rustup target list | grep "$(TARGET) (installed)") || rustup target add $(TARGET)
ifeq ($(COMP), no)
	@if command -v pacman >/dev/null 2>&1; then \
		if ! pacman -Q expect >/dev/null 2>&1; then \
			sudo pacman -S --noconfirm expect; \
		fi \
	elif command -v apt >/dev/null 2>&1; then \
		if ! dpkg -s expect >/dev/null 2>&1; then \
			sudo apt update && sudo apt install -y expect; \
		fi \
	else \
		echo "Error: System has neither pacman nor apt package manager."; \
		echo "Please install expect manually and ensure it's in PATH"; \
		exit 1; \
	fi
else
	@echo "COMP is not set to 'no', skipping environment setup"
endif

clean:
	@cargo clean
	@cd ../user && make clean

.PHONY: user update gdb new-gdb monitor .FORCE
