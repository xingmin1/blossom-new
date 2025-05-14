ARCHS = rv64
CURR_ARCH ?= la64
FS_MODE ?= ext4
BLK_MODE ?= virt
MODE ?= release

.PHONY: all $(ARCHS) clean

all: $(ARCHS)

$(ARCHS):
	@echo "Building for ARCH=$@"
	make ARCH=$@ -f make/$@.mk all BLK_MODE=${BLK_MODE} MODE=${MODE}

rv64-kernel-build-only:
	make -f make/rv64.mk build BLK_MODE=${BLK_MODE} MODE=${MODE}

rv64-debug:
	make -f make/rv64.mk all BLK_MODE=${BLK_MODE} MODE=debug

rv64-only:
	make -f make/rv64.mk all BLK_MODE=${BLK_MODE}

rv64-run-only:
	make -f make/rv64.mk runsimple

rv64-run:
	make -f make/rv64.mk comp

la64-only:
	make -f make/la64.mk all BLK_MODE=${BLK_MODE}

la64-run-only:
	make -f make/la64.mk runsimple

la64-run:
	make -f make/la64.mk comp

comp:
	make -f make/$(CURR_ARCH).mk comp

clean:
	for arch in $(ARCHS); do \
		make ARCH=$$arch -f make/$$arch.mk clean; \
	done

fs-img:
	make -f make/$(CURR_ARCH).mk fs-img FS_MODE=$(FS_MODE)

run:
	make -f make/$(CURR_ARCH).mk run

gdb:
	make -f make/$(CURR_ARCH).mk gdb

rv64-gdb: rv64-debug
	make -f make/rv64.mk comp-gdb