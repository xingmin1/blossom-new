ARCHS = rv64 la64
CURR_ARCH ?= la64
FS_MODE ?= ext4
BLK_MODE ?= virt
MODE ?= release

.PHONY: all $(ARCHS) clean

all: $(ARCHS)

$(ARCHS):
	@echo "Building for ARCH=$@"
	make ARCH=$@ -f make/$@.mk all MODE=${MODE}

rv64-kernel-build-only:
	make -f make/rv64.mk build BLK_MODE=${BLK_MODE} MODE=${MODE} LOG=${LOG}

rv64-debug:
	make -f make/rv64.mk all BLK_MODE=${BLK_MODE} MODE=debug

rv64-only:
	make -f make/rv64.mk all BLK_MODE=${BLK_MODE}

rv64-run-only:
	make -f make/rv64.mk runsimple

rv64-run:
	make -f make/rv64.mk comp

la64-only:
	make -f make/la64.mk all 

la64-kernel-build-only:
	make -f make/la64.mk build MODE=${MODE} LOG=${LOG}

la64-b-only:
	make -f make/la_board/la64board.mk all BLK_MODE=mem BOARD=2k1000

la64-b-run:
	make -f make/la_board/la64board.mk run-inner BLK_MODE=mem BOARD=2k1000

la64-debug:
	make -f make/la64.mk all MODE=debug	

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

la_board_clean:
	make -f make/la_board/la64board.mk clean

laclean:
	make -f make/la64.mk clean

fs-img:
	make -f make/$(CURR_ARCH).mk fs-img FS_MODE=$(FS_MODE)

run:
	make -f make/$(CURR_ARCH).mk run

gdb:
	make -f make/$(CURR_ARCH).mk gdb

rv64-gdb: rv64-debug
	make -f make/rv64.mk comp-gdb

la64-gdb: la64-debug
	make -f make/la64.mk comp-gdb	

rvchange:
	rm -f ../sdcard-rv.img
	cp ../../imgbk/sdcard-rv.img ../sdcard-rv.img

finalrvchange:
	rm -f ../sdcard-rv.img
	cp ../../testsuits-for-oskernel/sdcard-rv.img ../sdcard-rv.img

lachange:
	rm -f ../sdcard-la.img
	cp ../../imgbk/sdcard-la.img ../sdcard-la.img

finallachange:
	rm -f ../sdcard-la.img
	cp ../../testsuits-for-oskernel/sdcard-la.img ../sdcard-la.img