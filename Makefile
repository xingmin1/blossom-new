.DEFAULT_GOAL := all

ARCH ?= riscv64
PLATFORM ?= qemu-virt
CPUS ?= 4
MEMORY ?= $(if $(filter riscv64,$(ARCH)),512M,768M)
MODE ?= debug
PANIC ?= unwind
KTEST_ARGS ?=
FEATURES ?=
OUTPUT ?= $(if $(filter qemu-virt,$(PLATFORM)),build/$(ARCH),build/$(PLATFORM)/$(MODE))/kernel.elf
BOARD_OUTPUT ?= build/$(PLATFORM)/$(MODE)
RAMDISK ?=
OS_DTB ?=
BOARD_ROOT ?= $(if $(RAMDISK),/dev/ram0)
TEST ?=
ROOT ?= /dev/vda
ROOT_MODE ?= ro
ROOTDIR ?= /
INIT ?= /sbin/init
NET ?= none
NET_FORWARD ?=
EXTRA_ROOT ?=
IDENTITY ?= root
IMAGE ?= build/$(if $(filter extra,$(IDENTITY)),extra,system)-$(ARCH).img
MANAGED_CONTEST := $(if $(filter undefined,$(origin STAGING)),1,0)
STAGING ?= build/system-$(ARCH)-$(IDENTITY)
SOURCE ?=
EXTRA_IMAGE ?=
SCRATCH_IMAGE ?=
WORK_DIR ?= build/run-$(ARCH)
CONTEST_IMAGE ?=
CONTEST_BOOT ?= 0
RESUME ?= 0
REFRESH_SYSTEM ?= 0
CONTEST_PLAN ?= userland/contest/plan.toml
LLVM_MC ?= llvm-mc
USER_LD ?= rust-lld
USER_OUTPUTS := $(addprefix build/user-$(ARCH)/user-,smoke io clone signal futex lifecycle write writeback device-map pipe events tty jobs)
MUSL_CROSS ?= $(ARCH)-linux-musl-
MUSL_CFLAGS ?= -std=c11 -O2 -Wall -Wextra -static -fno-pie -no-pie
USERLAND_BUILD ?= build/busybox-$(ARCH)
USERLAND_ROOT ?= build/userland-$(ARCH)
MUSL_LIBC ?=
GLIBC_LIBDIR ?=
RUNTIME_OUTPUT ?= build/runtime-$(ARCH)
MUSL_LIBC_RV ?=
MUSL_LIBC_LA ?=
GLIBC_LIBDIR_RV ?=
GLIBC_LIBDIR_LA ?=
MUSL_CROSS_RV ?= riscv64-linux-musl-
MUSL_CROSS_LA ?= loongarch64-linux-musl-
USERLAND_SCRIPTS := $(patsubst tests/user/scripts/%,$(USERLAND_ROOT)/test/scripts/%,$(wildcard tests/user/scripts/*))
RUNNER_TARGET := $(if $(filter riscv64,$(ARCH)),riscv64gc-unknown-linux-musl,loongarch64-unknown-linux-musl)
RUNNER_TARGET_DIR ?= build/contest-run
RUNNER_OUTPUT := $(RUNNER_TARGET_DIR)/$(RUNNER_TARGET)/release/contest-run
RUNNER_SYSROOT ?=
RUNNER_LINKER ?= $(MUSL_CROSS)gcc
RUNNER_SYSROOT_RV ?= $(RUNNER_SYSROOT)
RUNNER_SYSROOT_LA ?= $(RUNNER_SYSROOT)
RUNNER_LINKER_RV ?= $(MUSL_CROSS_RV)gcc
RUNNER_LINKER_LA ?= $(MUSL_CROSS_LA)gcc
RUNNER_FLAGS := -Ctarget-feature=+crt-static -Crelocation-model=static -Clink-self-contained=yes -Clink-arg=-no-pie $(if $(RUNNER_SYSROOT),--sysroot $(abspath $(RUNNER_SYSROOT))) -Clinker=$(if $(findstring /,$(RUNNER_LINKER)),$(abspath $(RUNNER_LINKER)),$(RUNNER_LINKER))
LIBC_SOURCES := tests/user/libc-smoke.c $(addprefix tests/user/libc/,common.c lifecycle.c script.c file.c system.c limits.c file_time.c clock.c socket_timeout.c)
LIBC_HEADERS := tests/user/libc/common.h tests/user/libc/fp.h
NETWORK_SOURCES := tests/user/network-smoke.c $(addprefix tests/user/network/,common.c tcp.c udp.c unix.c options.c ipv6.c peer.c control.c)
NETWORK_HEADERS := tests/user/network/common.h

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

ifneq ($(words $(PLATFORM)),1)
$(error PLATFORM must name exactly one platform)
endif
ifeq ($(filter riscv64:qemu-virt riscv64:visionfive2 loongarch64:qemu-virt loongarch64:ls2k1000la,$(ARCH):$(PLATFORM)),)
$(error Unsupported ARCH/PLATFORM combination: $(ARCH)/$(PLATFORM))
endif
ifneq ($(PLATFORM),qemu-virt)
ifneq ($(filter run debug contest,$(MAKECMDGOALS)),)
$(error run/debug/contest require PLATFORM=qemu-virt; package-board-boot-images only generates offline files)
endif
ifneq ($(filter ktest-build ktest-list ktest,$(MAKECMDGOALS)),)
$(error ktest-build/ktest-list/ktest require PLATFORM=qemu-virt)
endif
ifneq ($(findstring ktest,$(FEATURES)),)
$(error Kernel test features require PLATFORM=qemu-virt)
endif
endif
ifeq ($(PLATFORM),qemu-virt)
ifneq ($(filter package-board-boot-images,$(MAKECMDGOALS)),)
$(error package-board-boot-images requires PLATFORM=visionfive2 or PLATFORM=ls2k1000la)
endif
endif

KERNEL_TARGET_DIR := $(if $(filter qemu-virt,$(PLATFORM)),os/target,build/$(PLATFORM)/target)
ifeq ($(PLATFORM),qemu-virt)
KTEST_TARGET_DIR := build/ktest-target
endif

ifneq ($(words $(CPUS)),1)
$(error CPUS 必须是一个核数)
endif
ifeq ($(filter $(CPUS),1 2 4),)
$(error 当前阶段 CPUS 只支持 1、2、4)
endif

ifneq ($(words $(ROOT_MODE)),1)
$(error ROOT_MODE 必须是一个挂载模式)
endif
ifneq ($(ROOT_MODE),$(filter ro rw,$(ROOT_MODE)))
$(error ROOT_MODE 只支持 ro 或 rw)
endif

ifneq ($(words $(RESUME) $(REFRESH_SYSTEM)),2)
$(error RESUME and REFRESH_SYSTEM must each be 0 or 1)
endif
ifneq ($(filter-out 0 1,$(RESUME) $(REFRESH_SYSTEM)),)
$(error RESUME and REFRESH_SYSTEM must each be 0 or 1)
endif
ifeq ($(REFRESH_SYSTEM),1)
ifneq ($(RESUME),1)
$(error REFRESH_SYSTEM requires RESUME=1)
endif
endif

ifeq ($(MODE),release)
CARGO_MODE := --release
else ifneq ($(MODE),debug)
$(error MODE 只支持 debug 或 release)
endif

CARGO_ARGS := --locked --target $(TARGET) $(CARGO_MODE) $(if $(FEATURES),--features $(FEATURES))

.PHONY: all delivery-rv delivery-la kernel-rv kernel-la build package-board-boot-images image run contest debug clean runtime runner work-clean help

all: delivery-rv delivery-la
	python3 tools/mkext4.py --delivery --contest-plan "$(CONTEST_PLAN)" --profile seed4k --size-mib 256 --rv-kernel build/delivery-riscv64/kernel.elf --la-kernel build/delivery-loongarch64/kernel.elf --rv-userland-root build/delivery-riscv64/userland --la-userland-root build/delivery-loongarch64/userland --rv-runtime-root build/runtime-riscv64 --la-runtime-root build/runtime-loongarch64

delivery-rv:
	$(MAKE) runtime ARCH=riscv64 PLATFORM=qemu-virt RUNTIME_OUTPUT=build/runtime-riscv64 MUSL_LIBC="$(MUSL_LIBC_RV)" GLIBC_LIBDIR="$(GLIBC_LIBDIR_RV)"
	$(MAKE) build userland ARCH=riscv64 PLATFORM=qemu-virt FEATURES= PANIC=abort OUTPUT=build/delivery-riscv64/kernel.elf USERLAND_BUILD=build/delivery-riscv64/busybox USERLAND_ROOT=build/delivery-riscv64/userland MUSL_CROSS="$(MUSL_CROSS_RV)" RUNNER_SYSROOT="$(RUNNER_SYSROOT_RV)" RUNNER_LINKER="$(RUNNER_LINKER_RV)"

delivery-la:
	$(MAKE) runtime ARCH=loongarch64 PLATFORM=qemu-virt RUNTIME_OUTPUT=build/runtime-loongarch64 MUSL_LIBC="$(MUSL_LIBC_LA)" GLIBC_LIBDIR="$(GLIBC_LIBDIR_LA)"
	$(MAKE) build userland ARCH=loongarch64 PLATFORM=qemu-virt FEATURES= PANIC=abort OUTPUT=build/delivery-loongarch64/kernel.elf USERLAND_BUILD=build/delivery-loongarch64/busybox USERLAND_ROOT=build/delivery-loongarch64/userland MUSL_CROSS="$(MUSL_CROSS_LA)" RUNNER_SYSROOT="$(RUNNER_SYSROOT_LA)" RUNNER_LINKER="$(RUNNER_LINKER_LA)"

kernel-rv:
	$(MAKE) build ARCH=riscv64 PLATFORM=qemu-virt FEATURES= PANIC=abort OUTPUT=build/riscv64/kernel.elf && cp build/riscv64/kernel.elf $@

kernel-la:
	$(MAKE) build ARCH=loongarch64 PLATFORM=qemu-virt FEATURES= PANIC=abort OUTPUT=build/loongarch64/kernel.elf && cp build/loongarch64/kernel.elf $@

build:
	cd os && BLOSSOM_PLATFORM="$(PLATFORM)" CARGO_TARGET_DIR="$(abspath $(KERNEL_TARGET_DIR))" cargo build $(CARGO_ARGS)
	mkdir -p "$(dir $(OUTPUT))"
	cp "$(KERNEL_TARGET_DIR)/$(TARGET)/$(MODE)/os" "$(OUTPUT)"

package-board-boot-images: build
	python3 tools/package_board_boot_images.py --arch $(ARCH) --target-platform $(PLATFORM) --kernel-elf "$(OUTPUT)" --output-directory "$(BOARD_OUTPUT)" $(if $(RAMDISK),--root-ext4-image "$(RAMDISK)") $(if $(OS_DTB),--os-dtb "$(OS_DTB)") $(if $(BOARD_ROOT),--root-device "$(BOARD_ROOT)") --root-mode "$(ROOT_MODE)" --init-program "$(INIT)"

runner:
	cd userland/contest-run && env -u CARGO_ENCODED_RUSTFLAGS RUSTUP_AUTO_INSTALL=0 CARGO_TARGET_DIR="$(abspath $(RUNNER_TARGET_DIR))" RUSTFLAGS="$(RUNNER_FLAGS)" cargo build --release --locked --offline --target $(RUNNER_TARGET)

runtime:
	python3 userland/runtime/prepare.py --arch $(ARCH) $(if $(MUSL_LIBC),--musl-libc "$(MUSL_LIBC)") $(if $(GLIBC_LIBDIR),--glibc-libdir "$(GLIBC_LIBDIR)") --output "$(RUNTIME_OUTPUT)"

image:
	@if [ -z "$(SOURCE)" ] && { { [ "$(CONTEST_BOOT)" = 1 ] && [ "$(MANAGED_CONTEST)" = 1 ]; } || [ ! -e "$(STAGING)" ]; }; then $(MAKE) runtime && $(MAKE) userland; fi
	python3 tools/mkext4.py --arch $(ARCH) --output "$(IMAGE)" --identity $(IDENTITY) --profile seed4k --size-mib 256 $(if $(SOURCE),--source "$(SOURCE)",--staging "$(STAGING)" --userland-root "$(USERLAND_ROOT)" --runtime-root "$(RUNTIME_OUTPUT)" $(if $(and $(filter 1,$(CONTEST_BOOT)),$(filter 1,$(MANAGED_CONTEST))),--managed-contest)) $(if $(filter 1,$(CONTEST_BOOT)),--contest-boot --contest-plan "$(CONTEST_PLAN)")

contest: IMAGE = build/contest-system-$(ARCH).img
contest: STAGING = build/contest-system-$(ARCH)
contest: WORK_DIR = build/contest-run-$(ARCH)
contest: CONTEST_BOOT := 1
ifneq ($(RESUME),1)
contest: image
else ifeq ($(REFRESH_SYSTEM),1)
contest: image
endif
ifeq ($(REFRESH_SYSTEM),1)
run debug: image
endif

run debug contest: build
	@case "$(RESUME)" in 0|1) ;; *) echo "RESUME must be 0 or 1"; exit 1;; esac
	$(if $(filter contest,$@),@test -n "$(CONTEST_IMAGE)" || { echo "CONTEST_IMAGE is required"; exit 1; })
	python3 tools/run.py --arch $(ARCH) --kernel "$(OUTPUT)" --cpus $(CPUS) --memory "$(MEMORY)" --qemu "$(QEMU)" --image "$(IMAGE)" --root "$(if $(filter contest,$@),/dev/vdb,$(ROOT))" --rootdir "$(ROOTDIR)" --root-mode "$(ROOT_MODE)" --init "$(INIT)" --net "$(NET)" --net-forward "$(NET_FORWARD)" --work-dir "$(WORK_DIR)" $(if $(filter contest,$@),--contest-image "$(CONTEST_IMAGE)") $(if $(filter 1,$(RESUME)),--resume) $(if $(filter 1,$(REFRESH_SYSTEM)),--refresh-system) $(if $(EXTRA_IMAGE),--extra-image "$(EXTRA_IMAGE)") $(if $(SCRATCH_IMAGE),--scratch-image "$(SCRATCH_IMAGE)") $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(filter debug,$@),--debug)


ifeq ($(PANIC),unwind)
KTEST_PROFILE := $(if $(filter release,$(MODE)),ktest-release,ktest-dev)
else ifeq ($(PANIC),abort)
KTEST_PROFILE := $(if $(filter release,$(MODE)),ktest-abort-release,ktest-abort-dev)
else
$(error PANIC 只支持 unwind 或 abort)
endif

.PHONY: ktest-build ktest-list ktest
ktest-build:
	cd os && BLOSSOM_PLATFORM="$(PLATFORM)" CARGO_TARGET_DIR="$(abspath $(KTEST_TARGET_DIR))" RUSTFLAGS="-Cpanic=$(PANIC) -Cforce-frame-pointers=yes -Cforce-unwind-tables=yes" cargo build --locked --target $(TARGET) --profile $(KTEST_PROFILE) --features ktest -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem
	mkdir -p "$(dir $(OUTPUT))"
	cp "$(KTEST_TARGET_DIR)/$(TARGET)/$(KTEST_PROFILE)/os" "$(OUTPUT)"

ktest-list:
	python3 tools/ktest.py list --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --panic $(PANIC) --root "$(ROOT)" --root-mode "$(ROOT_MODE)" $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(TEST),--glob "$(TEST)") $(KTEST_ARGS)

ktest:
	python3 tools/ktest.py run --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(MODE) --panic $(PANIC) --root "$(ROOT)" --root-mode "$(ROOT_MODE)" $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(TEST),--glob "$(TEST)") $(KTEST_ARGS)

.PHONY: user-tests
user-tests: $(USER_OUTPUTS)

build/user-$(ARCH)/user-%: tests/user/%-$(ARCH).S tests/user/smoke.ld
	mkdir -p "$(dir $@)"
	$(LLVM_MC) --triple=$(USER_TRIPLE) --mattr=$(USER_MATTR) --filetype=obj "$<" -o "$@.o"
	$(USER_LD) -flavor gnu -static --no-dynamic-linker --no-relax --build-id=none -z max-page-size=4096 -T tests/user/smoke.ld -m $(USER_EMULATION) "$@.o" -o "$@"

.PHONY: userland
userland: $(USERLAND_BUILD)/.config $(USERLAND_ROOT)/bin/libc-smoke $(USERLAND_ROOT)/bin/network-smoke $(USERLAND_ROOT)/bin/klog-read $(USERLAND_ROOT)/bin/ktrace-control $(USERLAND_ROOT)/bin/observation-smoke $(USERLAND_ROOT)/sbin/contest-run $(USERLAND_SCRIPTS)
	$(MAKE) -C dependency/busybox O="$(abspath $(USERLAND_BUILD))" KBUILD_OUTPUT="$(abspath $(USERLAND_BUILD))" ARCH="$(ARCH)" CROSS_COMPILE="$(MUSL_CROSS)" CONFIG_PREFIX="$(abspath $(USERLAND_ROOT))" install

$(USERLAND_ROOT)/bin/libc-smoke: $(LIBC_SOURCES) $(LIBC_HEADERS)
	mkdir -p "$(dir $@)"
	$(MUSL_CROSS)gcc $(MUSL_CFLAGS) $(LIBC_SOURCES) -o "$@"

$(USERLAND_ROOT)/bin/network-smoke: $(NETWORK_SOURCES) $(NETWORK_HEADERS)
	mkdir -p "$(dir $@)"
	$(MUSL_CROSS)gcc $(MUSL_CFLAGS) $(NETWORK_SOURCES) -o "$@"

$(USERLAND_ROOT)/bin/klog-read: userland/tools/klog-read.c userland/include/blossom-trace.h
	mkdir -p "$(dir $@)"
	$(MUSL_CROSS)gcc $(MUSL_CFLAGS) -Iuserland/include "$<" -o "$@"

$(USERLAND_ROOT)/bin/ktrace-control: userland/tools/ktrace-control.c userland/include/blossom-trace.h
	mkdir -p "$(dir $@)"
	$(MUSL_CROSS)gcc $(MUSL_CFLAGS) -Iuserland/include "$<" -o "$@"

$(USERLAND_ROOT)/bin/observation-smoke: tests/user/observation-smoke.c userland/include/blossom-trace.h
	mkdir -p "$(dir $@)"
	$(MUSL_CROSS)gcc $(MUSL_CFLAGS) -Iuserland/include "$<" -o "$@"

$(USERLAND_ROOT)/sbin/contest-run: runner
	mkdir -p "$(dir $@)"
	install -m 755 "$(RUNNER_OUTPUT)" "$@"

$(USERLAND_ROOT)/test/scripts/%: tests/user/scripts/%
	mkdir -p "$(dir $@)"
	install -m 755 "$<" "$@"


$(USERLAND_ROOT)/test/scripts/no-exec: tests/user/scripts/no-exec
	mkdir -p "$(dir $@)"
	install -m 644 "$<" "$@"

.DELETE_ON_ERROR: $(USERLAND_BUILD)/.config
$(USERLAND_BUILD)/.config: userland/busybox.config
	mkdir -p "$(USERLAND_BUILD)"
	cp "$<" "$@"
	$(MAKE) -C dependency/busybox O="$(abspath $(USERLAND_BUILD))" KBUILD_OUTPUT="$(abspath $(USERLAND_BUILD))" ARCH="$(ARCH)" CROSS_COMPILE="$(MUSL_CROSS)" oldconfig

clean:
	cd os && CARGO_TARGET_DIR="$(abspath $(KERNEL_TARGET_DIR))" cargo clean
	cd userland/contest-run && RUSTUP_AUTO_INSTALL=0 CARGO_TARGET_DIR="$(abspath $(RUNNER_TARGET_DIR))" cargo clean --offline
	$(MAKE) -C dependency/busybox O="$(abspath $(USERLAND_BUILD))" KBUILD_OUTPUT="$(abspath $(USERLAND_BUILD))" ARCH="$(ARCH)" CROSS_COMPILE="$(MUSL_CROSS)" clean
	rm -f $(USER_OUTPUTS) $(addsuffix .o,$(USER_OUTPUTS)) "$(OUTPUT)" $(if $(filter qemu-virt,$(PLATFORM)),kernel-rv kernel-la)
	rm -f $(addprefix $(USERLAND_ROOT)/bin/,libc-smoke network-smoke klog-read ktrace-control observation-smoke) "$(USERLAND_ROOT)/sbin/contest-run" "$(USERLAND_ROOT)/bin/busybox" "$(USERLAND_BUILD)/busybox" $(if $(filter qemu-virt,$(PLATFORM)),build/delivery-riscv64/kernel.elf build/delivery-loongarch64/kernel.elf)

work-clean:
	python3 tools/run.py --work-clean --work-dir "$(WORK_DIR)"

help:
	@printf '%s\n' \
	  'build ARCH=riscv64|loongarch64 PLATFORM=qemu-virt Kernel only (MODE=debug|release).' \
	  '  Board combinations: riscv64/visionfive2, loongarch64/ls2k1000la.' \
	  '  Board Cargo/output directories are separate from official QEMU kernels.' \
	  'package-board-boot-images PLATFORM=... BOARD_ROOT=/dev/...  Offline raw/uImage/boot.env.' \
	  '  RAMDISK=<raw-ext4> defaults BOARD_ROOT=/dev/ram0; OS_DTB=<OS-dtb> is optional.' \
	  '  BOARD_OUTPUT selects the artifact directory; no board or media operations.' \
	  '  SOURCE remains the image input; IMAGE is the volume output; OUTPUT is the ELF.' \
	  '  Single-architecture board volumes use rootdir=/; prepare them with make image.' \
	  'runner ARCH=...                    Offline release Linux musl runner.' \
	  '  RUNNER_SYSROOT=... RUNNER_LINKER=... override prepared compiler inputs.' \
	  'userland ARCH=...                  BusyBox, runner, tools and smoke programs.' \
	  'image ARCH=...                     Ordinary staging; SOURCE is never refilled.' \
	  'contest CONTEST_IMAGE=...          Fresh contest copies with current runner/config.' \
	  '  CONTEST_PLAN=userland/contest/plan.toml selects an entry inside the full tree.' \
	  '  RESUME=1                         Keep all existing working disks.' \
	  '  RESUME=1 REFRESH_SYSTEM=1         Replace only root.raw under the work lock.' \
	  'all                               Export kernel-rv/kernel-la/disk.img/disk-la.img.' \
	  'ktest-list / ktest TEST=...         Named kernel tests; PLATFORM=qemu-virt only.' \
	  'clean                             Rebuildable outputs only; keep disks and reports.' \
	  'work-clean WORK_DIR=...            Remove known working disks under their lock.'
