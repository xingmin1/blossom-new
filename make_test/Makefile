CROSS_COMPILE ?= riscv64-linux-gnu-
CC := $(CROSS_COMPILE)gcc
CFLAGS := -Wall -Wextra -g

all: copyfilerange_test splice_test interrupts_test1 interrupts_test2

copyfilerange_test: copyfilerange/test.c
	$(CC) $(CFLAGS) -DCASE=0 -o $@ $<

splice_test: splice/test.c
	$(CC) $(CFLAGS) -static -O3 -o $@ $<

interrupts_test1: interrupts/test1.c
	$(CC) $(CFLAGS) -static -O3 -o $@ $<

interrupts_test2: interrupts/test2.c
	$(CC) $(CFLAGS) -static -O3 -o $@ $<

clean:
	rm -f copyfilerange_test splice_test interrupts_test1 interrupts_test2

.PHONY: all clean
