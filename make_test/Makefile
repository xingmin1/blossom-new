CROSS_COMPILE ?= riscv64-linux-gnu-
CC := $(CROSS_COMPILE)gcc
CFLAGS := -Wall -Wextra -g

all: copyfilerange_test_1 copyfilerange_test_2 copyfilerange_test_3 copyfilerange_test_4 splice_test interrupts_test1 interrupts_test2

copyfilerange_test_%: copyfilerange/test.c
	$(CC) $(CFLAGS) -static -O3 -Wall -DCASE=$* -o $@ $<

splice_test: splice/test.c
	$(CC) $(CFLAGS) -static -O3 -o $@ $<

interrupts_test1: interrupts/test1.c
	$(CC) $(CFLAGS) -static -O3 -o $@ $<

interrupts_test2: interrupts/test2.c
	$(CC) $(CFLAGS) -static -O3 -o $@ $<

clean:
	rm -f copyfilerange_test_1 copyfilerange_test_2 copyfilerange_test_3 copyfilerange_test_4 splice_test interrupts_test1 interrupts_test2

.PHONY: all clean
