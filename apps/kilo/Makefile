# 源文件
SRC := kilo.c

# 编译选项
CFLAGS := -Wall -W -pedantic -std=c99 -static -fno-stack-protector

# 交叉编译器（用你 PATH 里的名字）
LA64_CC  := loongarch64-linux-gnu-gcc
RISCV_CC := riscv64-linux-gnu-gcc

# 输出目录
OUTDIR := build
LA64_OUT  := $(OUTDIR)/kilo-la64
RISCV_OUT := $(OUTDIR)/kilo-riscv64

# 默认目标：同时生成两个架构的版本
all: $(LA64_OUT) $(RISCV_OUT)

$(LA64_OUT): $(SRC)
	mkdir -p $(OUTDIR)
	$(LA64_CC) $(CFLAGS) -o $@ $<

$(RISCV_OUT): $(SRC)
	mkdir -p $(OUTDIR)
	$(RISCV_CC) $(CFLAGS) -o $@ $<

clean:
	rm -rf $(OUTDIR)
