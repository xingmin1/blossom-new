# 源文件
SRC := src/main.cpp src/tetromino.cpp src/game.cpp

# 编译选项
CXXFLAGS := -Wall -W -pedantic -std=c++11 -static -fno-stack-protector

# 交叉编译器（用你 PATH 里的名字）
LA64_CXX  := loongarch64-linux-gnu-g++
RISCV_CXX := riscv64-linux-gnu-g++

# 输出目录
OUTDIR := build
LA64_OUT  := $(OUTDIR)/tetris-la64
RISCV_OUT := $(OUTDIR)/tetris-riscv64

# 默认目标：同时生成两个架构的版本
all: $(LA64_OUT) $(RISCV_OUT)

$(LA64_OUT): $(SRC)
	mkdir -p $(OUTDIR)
	$(LA64_CXX) $(CXXFLAGS) -o $@ $(SRC) $(LDLIBS)

$(RISCV_OUT): $(SRC)
	mkdir -p $(OUTDIR)
	$(RISCV_CXX) $(CXXFLAGS) -o $@ $(SRC) $(LDLIBS)

clean:
	rm -rf $(OUTDIR)
