.DEFAULT_GOAL := all

ARCH ?= riscv64
TARGET_PLATFORM ?= qemu-virt
KERNEL_BUILD_PROFILE ?= debug
KERNEL_ELF_OUTPUT ?= build/kernel/$(ARCH)/$(TARGET_PLATFORM)/$(KERNEL_BUILD_PROFILE)/kernel.elf
KERNEL_CARGO_TARGET_DIR ?= build/kernel/$(ARCH)/$(TARGET_PLATFORM)/target
KTEST_CARGO_TARGET_DIR ?= build/ktest-target
KTEST_PANIC_STRATEGY ?= unwind
KTEST_ARGS ?=
QEMU_ARGS ?=
TEST ?=
CPUS ?= 4
MEMORY ?= $(if $(filter riscv64,$(ARCH)),512M,768M)
ROOT ?= /dev/vda
ROOT_MODE ?= ro
EXTRA_ROOT ?=

ROOTFS_OUTPUT_DIR ?= build/rootfs/$(ARCH)
ROOTFS_TREE_DIR ?= $(ROOTFS_OUTPUT_DIR)
ROOTFS_INIT_CONFIG ?=
SYSTEM_ROOT_EXT4_IMAGE ?= build/system-$(ARCH).img
EXT4_PROFILE ?= seed4k
EXT4_SIZE_MIB ?= 256
GUEST_USERLAND_OUTPUT_DIR ?= build/userland-$(ARCH)
RUNTIME_LIBRARIES_OUTPUT_DIR ?= build/runtime-$(ARCH)
BUSYBOX_BUILD_DIR ?= build/busybox-$(ARCH)
MUSL_LIBC ?=
GLIBC_LIBDIR ?=
MUSL_LIBC_RV ?=
MUSL_LIBC_LA ?=
GLIBC_LIBDIR_RV ?=
GLIBC_LIBDIR_LA ?=
MUSL_CROSS ?= $(ARCH)-linux-musl-
MUSL_CROSS_RV ?= riscv64-linux-musl-
MUSL_CROSS_LA ?= loongarch64-linux-musl-
MUSL_CFLAGS ?= -std=c11 -O2 -Wall -Wextra -static -fno-pie -no-pie
LLVM_MC ?= llvm-mc
USER_LD ?= rust-lld
USER_OUTPUTS := $(addprefix build/user-$(ARCH)/user-,smoke io clone signal futex lifecycle write writeback device-map pipe events tty jobs)
RUNNER_TARGET := $(if $(filter riscv64,$(ARCH)),riscv64gc-unknown-linux-musl,loongarch64-unknown-linux-musl)
RUNNER_TARGET_DIR ?= build/contest-run
RUNNER_OUTPUT := $(RUNNER_TARGET_DIR)/$(RUNNER_TARGET)/release/contest-run
RUNNER_SYSROOT ?=
RUNNER_LINKER ?= $(MUSL_CROSS)gcc
RUNNER_SYSROOT_RV ?= $(RUNNER_SYSROOT)
RUNNER_SYSROOT_LA ?= $(RUNNER_SYSROOT)
RUNNER_LINKER_RV ?= $(MUSL_CROSS_RV)gcc
RUNNER_LINKER_LA ?= $(MUSL_CROSS_LA)gcc
BOARD_BOOT_IMAGES_OUTPUT_DIR ?= build/board/$(ARCH)/$(TARGET_PLATFORM)/$(KERNEL_BUILD_PROFILE)
BOARD_ROOT_EXT4_IMAGE ?=
BOARD_OS_DTB ?=
BOARD_ROOT_DEVICE ?=
BOARD_ROOT_MODE ?= ro
BOARD_INIT_PROGRAM ?= /sbin/init
LIBC_SOURCES := tests/user/libc-smoke.c $(addprefix tests/user/libc/,common.c lifecycle.c script.c file.c system.c limits.c file_time.c clock.c socket_timeout.c)
LIBC_HEADERS := tests/user/libc/common.h tests/user/libc/fp.h
NETWORK_SOURCES := tests/user/network-smoke.c $(addprefix tests/user/network/,common.c tcp.c udp.c unix.c options.c ipv6.c peer.c control.c)
NETWORK_HEADERS := tests/user/network/common.h

empty :=
space := $(empty) $(empty)
make-path = $(subst $(space),\$(space),$(1))
absolute-path = $(if $(filter /%,$(firstword $(1))),$(1),$(CURDIR)/$(1))

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
USER_TRIPLE := riscv64-unknown-linux-gnu
USER_MATTR := +m,+a,+f,+d,-c,-v
USER_EMULATION := elf64lriscv
QEMU_KERNEL_EXPORT := kernel-rv
QEMU_ROOT_EXT4_IMAGE := disk.img
else ifeq ($(ARCH),loongarch64)
TARGET := loongarch64-unknown-none
USER_TRIPLE := loongarch64-unknown-linux-gnu
USER_MATTR := +f,+d,-lsx,-lasx
USER_EMULATION := elf64loongarch
QEMU_KERNEL_EXPORT := kernel-la
QEMU_ROOT_EXT4_IMAGE := disk-la.img
else
$(error ARCH must be riscv64 or loongarch64)
endif

ifneq ($(words $(TARGET_PLATFORM)),1)
$(error TARGET_PLATFORM must name exactly one platform)
endif
ifeq ($(filter riscv64:qemu-virt riscv64:visionfive2 loongarch64:qemu-virt loongarch64:ls2k1000la,$(ARCH):$(TARGET_PLATFORM)),)
$(error Unsupported ARCH/TARGET_PLATFORM combination: $(ARCH)/$(TARGET_PLATFORM))
endif
ifneq ($(TARGET_PLATFORM),qemu-virt)
ifneq ($(filter ktest-build ktest-list ktest,$(MAKECMDGOALS)),)
$(error ktest-build/ktest-list/ktest require TARGET_PLATFORM=qemu-virt)
endif
endif
ifeq ($(TARGET_PLATFORM),qemu-virt)
ifneq ($(filter package-board-boot-images,$(MAKECMDGOALS)),)
$(error package-board-boot-images requires TARGET_PLATFORM=visionfive2 or TARGET_PLATFORM=ls2k1000la)
endif
endif

ifeq ($(KERNEL_BUILD_PROFILE),release)
CARGO_PROFILE_ARG := --release
else ifneq ($(KERNEL_BUILD_PROFILE),debug)
$(error KERNEL_BUILD_PROFILE must be debug or release)
endif
ifeq ($(KTEST_PANIC_STRATEGY),unwind)
KTEST_PROFILE := $(if $(filter release,$(KERNEL_BUILD_PROFILE)),ktest-release,ktest-dev)
else ifeq ($(KTEST_PANIC_STRATEGY),abort)
KTEST_PROFILE := $(if $(filter release,$(KERNEL_BUILD_PROFILE)),ktest-abort-release,ktest-abort-dev)
else
$(error KTEST_PANIC_STRATEGY must be unwind or abort)
endif

.PHONY: all build-kernel build-guest-userland prepare-guest-runtime-libraries assemble-root-filesystem create-ext4-image build-qemu-artifacts package-board-boot-images
.PHONY: build-contest-runner user-tests ktest-build ktest-list ktest run-qemu clean-build-artifacts clean-qemu-working-disks help

all:
	$(MAKE) build-qemu-artifacts ARCH=riscv64 TARGET_PLATFORM=qemu-virt MUSL_CROSS="$(MUSL_CROSS_RV)" MUSL_LIBC="$(MUSL_LIBC_RV)" GLIBC_LIBDIR="$(GLIBC_LIBDIR_RV)" RUNNER_SYSROOT="$(RUNNER_SYSROOT_RV)" RUNNER_LINKER="$(RUNNER_LINKER_RV)" ROOTFS_OUTPUT_DIR=build/rootfs/riscv64 GUEST_USERLAND_OUTPUT_DIR=build/userland-riscv64 RUNTIME_LIBRARIES_OUTPUT_DIR=build/runtime-riscv64 BUSYBOX_BUILD_DIR=build/busybox-riscv64
	$(MAKE) build-qemu-artifacts ARCH=loongarch64 TARGET_PLATFORM=qemu-virt MUSL_CROSS="$(MUSL_CROSS_LA)" MUSL_LIBC="$(MUSL_LIBC_LA)" GLIBC_LIBDIR="$(GLIBC_LIBDIR_LA)" RUNNER_SYSROOT="$(RUNNER_SYSROOT_LA)" RUNNER_LINKER="$(RUNNER_LINKER_LA)" ROOTFS_OUTPUT_DIR=build/rootfs/loongarch64 GUEST_USERLAND_OUTPUT_DIR=build/userland-loongarch64 RUNTIME_LIBRARIES_OUTPUT_DIR=build/runtime-loongarch64 BUSYBOX_BUILD_DIR=build/busybox-loongarch64

build-kernel:
	cd os && env -u CARGO_ENCODED_RUSTFLAGS BLOSSOM_PLATFORM="$(TARGET_PLATFORM)" CARGO_TARGET_DIR="$(call absolute-path,$(KERNEL_CARGO_TARGET_DIR))" RUSTFLAGS="-Cpanic=abort -Cforce-frame-pointers=yes" cargo build --locked --target $(TARGET) $(CARGO_PROFILE_ARG)
	mkdir -p "$$(dirname -- "$(KERNEL_ELF_OUTPUT)")"
	cp "$(KERNEL_CARGO_TARGET_DIR)/$(TARGET)/$(KERNEL_BUILD_PROFILE)/os" "$(KERNEL_ELF_OUTPUT)"

build-qemu-artifacts:
	$(MAKE) build-kernel TARGET_PLATFORM=qemu-virt KERNEL_BUILD_PROFILE=release KERNEL_CARGO_TARGET_DIR=build/kernel/$(ARCH)/qemu-virt/target KERNEL_ELF_OUTPUT=build/kernel/$(ARCH)/qemu-virt/release/kernel.elf
	$(MAKE) assemble-root-filesystem TARGET_PLATFORM=qemu-virt
	$(MAKE) create-ext4-image ROOTFS_TREE_DIR="$(ROOTFS_OUTPUT_DIR)" SYSTEM_ROOT_EXT4_IMAGE="$(QEMU_ROOT_EXT4_IMAGE)"
	@temporary="$$(mktemp -- "./.$(QEMU_KERNEL_EXPORT).XXXXXX")" || exit $$?; \
	trap 'status=$$?; rm -f -- "$$temporary"; exit "$$status"' EXIT; \
	trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; \
	cp --preserve=mode -- "build/kernel/$(ARCH)/qemu-virt/release/kernel.elf" "$$temporary" || exit $$?; \
	mv -fT -- "$$temporary" "$(QEMU_KERNEL_EXPORT)" || exit $$?

prepare-guest-runtime-libraries:
	python3 userland/runtime/prepare_runtime_libraries.py --arch $(ARCH) $(if $(MUSL_LIBC),--musl-libc "$(MUSL_LIBC)") $(if $(GLIBC_LIBDIR),--glibc-libdir "$(GLIBC_LIBDIR)") --output-directory "$(RUNTIME_LIBRARIES_OUTPUT_DIR)"

assemble-root-filesystem: build-guest-userland prepare-guest-runtime-libraries
	python3 tools/assemble_rootfs.py --arch $(ARCH) --guest-userland-directory "$(GUEST_USERLAND_OUTPUT_DIR)" --runtime-libraries-directory "$(RUNTIME_LIBRARIES_OUTPUT_DIR)" --output-directory "$(ROOTFS_OUTPUT_DIR)" $(if $(ROOTFS_INIT_CONFIG),--init-config "$(ROOTFS_INIT_CONFIG)")

create-ext4-image:
	python3 tools/create_ext4_image.py --source-tree "$(ROOTFS_TREE_DIR)" --output-image "$(SYSTEM_ROOT_EXT4_IMAGE)" --profile "$(EXT4_PROFILE)" --size-mib "$(EXT4_SIZE_MIB)"

package-board-boot-images: build-kernel
	python3 tools/package_board_boot_images.py --arch $(ARCH) --target-platform $(TARGET_PLATFORM) --kernel-elf "$(KERNEL_ELF_OUTPUT)" --output-directory "$(BOARD_BOOT_IMAGES_OUTPUT_DIR)" $(if $(BOARD_ROOT_EXT4_IMAGE),--root-ext4-image "$(BOARD_ROOT_EXT4_IMAGE)") $(if $(BOARD_OS_DTB),--os-dtb "$(BOARD_OS_DTB)") $(if $(BOARD_ROOT_DEVICE),--root-device "$(BOARD_ROOT_DEVICE)") --root-mode "$(BOARD_ROOT_MODE)" --init-program "$(BOARD_INIT_PROGRAM)"

run-qemu:
	python3 tools/run_qemu.py --arch "$(ARCH)" $(QEMU_ARGS)

clean-qemu-working-disks:
	python3 tools/working_disks.py clean --arch "$(ARCH)" $(QEMU_ARGS)

build-contest-runner:
	cd userland/contest-run && env -u RUSTFLAGS RUSTUP_AUTO_INSTALL=0 CARGO_TARGET_DIR="$(call absolute-path,$(RUNNER_TARGET_DIR))" CARGO_ENCODED_RUSTFLAGS="$$(printf '%s\037' '-Ctarget-feature=+crt-static' '-Crelocation-model=static' '-Clink-self-contained=yes' '-Clink-arg=-no-pie' $(if $(RUNNER_SYSROOT),'--sysroot' "$(call absolute-path,$(RUNNER_SYSROOT))"); printf '%s' "-Clinker=$(if $(findstring /,$(RUNNER_LINKER)),$(call absolute-path,$(RUNNER_LINKER)),$(RUNNER_LINKER))")" cargo build --bin contest-run --release --locked --offline --target $(RUNNER_TARGET)

ktest-build:
	cd os && env -u CARGO_ENCODED_RUSTFLAGS BLOSSOM_PLATFORM="$(TARGET_PLATFORM)" CARGO_TARGET_DIR="$(call absolute-path,$(KTEST_CARGO_TARGET_DIR))" RUSTFLAGS="-Cpanic=$(KTEST_PANIC_STRATEGY) -Cforce-frame-pointers=yes -Cforce-unwind-tables=yes" cargo build --locked --target $(TARGET) --profile $(KTEST_PROFILE) --features ktest -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem
	mkdir -p "$$(dirname -- "$(KERNEL_ELF_OUTPUT)")"
	cp "$(KTEST_CARGO_TARGET_DIR)/$(TARGET)/$(KTEST_PROFILE)/os" "$(KERNEL_ELF_OUTPUT)"

ktest-list:
	python3 tools/ktest.py list --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(KERNEL_BUILD_PROFILE) --panic $(KTEST_PANIC_STRATEGY) --root "$(ROOT)" --root-mode "$(ROOT_MODE)" $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(TEST),--glob "$(TEST)") $(KTEST_ARGS)

ktest:
	python3 tools/ktest.py run --arch $(ARCH) --cpus $(CPUS) --memory "$(MEMORY)" --mode $(KERNEL_BUILD_PROFILE) --panic $(KTEST_PANIC_STRATEGY) --root "$(ROOT)" --root-mode "$(ROOT_MODE)" $(if $(EXTRA_ROOT),--extra-root "$(EXTRA_ROOT)") $(if $(TEST),--glob "$(TEST)") $(KTEST_ARGS)

user-tests: $(USER_OUTPUTS)

build/user-$(ARCH)/user-%: tests/user/%-$(ARCH).S tests/user/smoke.ld
	mkdir -p "$(@D)"
	"$(LLVM_MC)" --triple=$(USER_TRIPLE) --mattr=$(USER_MATTR) --filetype=obj "$<" -o "$@.o"
	"$(USER_LD)" -flavor gnu -static --no-dynamic-linker --no-relax --build-id=none -z max-page-size=4096 -T tests/user/smoke.ld -m $(USER_EMULATION) "$@.o" -o "$@"

build-guest-userland: $(call make-path,$(BUSYBOX_BUILD_DIR)/.config) $(foreach program,libc-smoke network-smoke klog-read ktrace-control observation-smoke,$(call make-path,$(GUEST_USERLAND_OUTPUT_DIR)/bin/$(program))) build-contest-runner
	$(MAKE) -C dependency/busybox O="$(call absolute-path,$(BUSYBOX_BUILD_DIR))" KBUILD_OUTPUT="$(call absolute-path,$(BUSYBOX_BUILD_DIR))" ARCH="$(ARCH)" CROSS_COMPILE="$(MUSL_CROSS)" CONFIG_PREFIX="$(call absolute-path,$(GUEST_USERLAND_OUTPUT_DIR))" install
	mkdir -p "$(GUEST_USERLAND_OUTPUT_DIR)/sbin"
	install -m 755 "$(RUNNER_OUTPUT)" "$(GUEST_USERLAND_OUTPUT_DIR)/sbin/contest-run"
	rm -rf "$(GUEST_USERLAND_OUTPUT_DIR)/test/scripts"
	mkdir -p "$(GUEST_USERLAND_OUTPUT_DIR)/test/scripts"
	@for source in tests/user/scripts/*; do \
		mode=755; [ "$${source##*/}" != no-exec ] || mode=644; \
		install -m "$$mode" "$$source" "$(GUEST_USERLAND_OUTPUT_DIR)/test/scripts/" || exit; \
	done

$(call make-path,$(GUEST_USERLAND_OUTPUT_DIR)/bin/libc-smoke): $(LIBC_SOURCES) $(LIBC_HEADERS)
	mkdir -p "$$(dirname -- "$@")"
	"$(MUSL_CROSS)gcc" $(MUSL_CFLAGS) $(LIBC_SOURCES) -o "$@"

$(call make-path,$(GUEST_USERLAND_OUTPUT_DIR)/bin/network-smoke): $(NETWORK_SOURCES) $(NETWORK_HEADERS)
	mkdir -p "$$(dirname -- "$@")"
	"$(MUSL_CROSS)gcc" $(MUSL_CFLAGS) $(NETWORK_SOURCES) -o "$@"

$(call make-path,$(GUEST_USERLAND_OUTPUT_DIR)/bin/klog-read): userland/tools/klog-read.c userland/include/blossom-trace.h
	mkdir -p "$$(dirname -- "$@")"
	"$(MUSL_CROSS)gcc" $(MUSL_CFLAGS) -Iuserland/include "$<" -o "$@"

$(call make-path,$(GUEST_USERLAND_OUTPUT_DIR)/bin/ktrace-control): userland/tools/ktrace-control.c userland/include/blossom-trace.h
	mkdir -p "$$(dirname -- "$@")"
	"$(MUSL_CROSS)gcc" $(MUSL_CFLAGS) -Iuserland/include "$<" -o "$@"

$(call make-path,$(GUEST_USERLAND_OUTPUT_DIR)/bin/observation-smoke): tests/user/observation-smoke.c userland/include/blossom-trace.h
	mkdir -p "$$(dirname -- "$@")"
	"$(MUSL_CROSS)gcc" $(MUSL_CFLAGS) -Iuserland/include "$<" -o "$@"

.DELETE_ON_ERROR: $(call make-path,$(BUSYBOX_BUILD_DIR)/.config)
$(call make-path,$(BUSYBOX_BUILD_DIR)/.config): userland/busybox.config
	mkdir -p "$(BUSYBOX_BUILD_DIR)"
	cp "$<" "$@"
	$(MAKE) -C dependency/busybox O="$(call absolute-path,$(BUSYBOX_BUILD_DIR))" KBUILD_OUTPUT="$(call absolute-path,$(BUSYBOX_BUILD_DIR))" ARCH="$(ARCH)" CROSS_COMPILE="$(MUSL_CROSS)" oldconfig

clean-build-artifacts:
	cd os && CARGO_TARGET_DIR="$(call absolute-path,$(KERNEL_CARGO_TARGET_DIR))" cargo clean
	cd userland/contest-run && RUSTUP_AUTO_INSTALL=0 CARGO_TARGET_DIR="$(call absolute-path,$(RUNNER_TARGET_DIR))" cargo clean --offline
	$(MAKE) -C dependency/busybox O="$(call absolute-path,$(BUSYBOX_BUILD_DIR))" KBUILD_OUTPUT="$(call absolute-path,$(BUSYBOX_BUILD_DIR))" ARCH="$(ARCH)" CROSS_COMPILE="$(MUSL_CROSS)" clean
	rm -f $(USER_OUTPUTS) $(addsuffix .o,$(USER_OUTPUTS)) "$(KERNEL_ELF_OUTPUT)"
	@for program in libc-smoke network-smoke klog-read ktrace-control observation-smoke busybox; do \
		rm -f "$(GUEST_USERLAND_OUTPUT_DIR)/bin/$$program"; \
	done
	rm -f "$(GUEST_USERLAND_OUTPUT_DIR)/sbin/contest-run"

help:
	@printf '%s\n' \
	  'build-kernel ARCH=riscv64|loongarch64 TARGET_PLATFORM=qemu-virt' \
	  '  Kernel only; KERNEL_BUILD_PROFILE=debug|release defaults to debug, panic=abort.' \
	  '  KERNEL_ELF_OUTPUT defaults to build/kernel/<arch>/<platform>/<profile>/kernel.elf.' \
	  '  Board combinations: riscv64/visionfive2 and loongarch64/ls2k1000la.' \
	  'build-guest-userland                Incremental BusyBox/C and release contest-run.' \
	  '  GUEST_USERLAND_OUTPUT_DIR, BUSYBOX_BUILD_DIR, MUSL_CROSS select actual inputs/outputs.' \
	  '  RUNNER_SYSROOT and RUNNER_LINKER select prepared Rust std compiler inputs.' \
	  'prepare-guest-runtime-libraries     Copy checked local libc sources to RUNTIME_LIBRARIES_OUTPUT_DIR.' \
	  'assemble-root-filesystem            Build userland/libraries, then replace ROOTFS_OUTPUT_DIR.' \
	  '  ROOTFS_INIT_CONFIG optionally selects an ordinary inittab file.' \
	  'create-ext4-image                   Only consume ROOTFS_TREE_DIR; write SYSTEM_ROOT_EXT4_IMAGE.' \
	  'build-qemu-artifacts ARCH=...        Fixed release/abort QEMU kernel and one single-root disk.' \
	  'all                                Export kernel-rv/kernel-la/disk.img/disk-la.img.' \
	  'package-board-boot-images           Build board kernel and generate offline boot files.' \
	  '  BOARD_BOOT_IMAGES_OUTPUT_DIR, BOARD_ROOT_EXT4_IMAGE, BOARD_OS_DTB,' \
	  '  BOARD_ROOT_DEVICE, BOARD_ROOT_MODE and BOARD_INIT_PROGRAM map directly to packaging.' \
	  'run-qemu ARCH=...                   Run existing inputs; QEMU_ARGS passes CLI options unchanged.' \
	  '  tools/run_qemu.py --help lists session, disk, network and debugger options.' \
	  'clean-qemu-working-disks ARCH=...   Remove inactive root.raw/contest.raw under their lock.' \
	  '  QEMU_ARGS may select --working-disk-dir; keep sources, directory and unrelated files.' \
	  'user-tests                         Build the existing raw user ELF programs.' \
	  'ktest-build / ktest-list / ktest     QEMU only; KERNEL_BUILD_PROFILE, KTEST_PANIC_STRATEGY.' \
	  '  TEST selects a glob; KTEST_ARGS passes other Host options; CPUS/MEMORY are VM-only.' \
	  'clean-build-artifacts              Rebuildable compiler outputs only; keep disks and reports.'
