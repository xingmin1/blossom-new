use core::arch::asm;
use core::arch::naked_asm;

use crate::config::KERNEL_STACK_SIZE;

#[naked]
#[no_mangle]
#[link_section = ".text.entry"]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        // 以下内容看不懂的话
        // 可以去看龙架构手册卷一 v1.11
        r"
                                        # 0x180 + n 为 DMWn 寄存器位置
        ori         $t0, $zero, 0x1     # 配置 CSR_DMWn_PLV0，0x1代表PLV0下允许直接映射模式
        # -2048 转为 12 位补码即为
        # 0b1000_0000_0000 = 0x800
        lu52i.d     $t0, $t0, -2048   # UC Uncached, PLV0, 0x8000_xxxx_xxxx_xxxx
        csrwr       $t0, 0x180          # 状态控制寄存器CSR_DMW0的地址（看龙架构手册）
        ori         $t0, $zero, 0x11    # CSR_DMWn_MAT | CSR_DMWn_PLV0
                                        # -1792 转为 12 位补码即为
                                        # 0b1001_0000_0000 = 0x900
        lu52i.d     $t0, $t0, -1792   # CA Cached, PLV0, 0x9000_xxxx_xxxx_xxxx
        csrwr       $t0, 0x181          # 状态控制寄存器CSR_DMW1的地址
        # 但是用户态的还没处理好，高位加9000_0000的话，会出现地址访问问题，
        # 所以暂时把高位的VSEG段也就是DMW寄存器里的[63:60]位置都改成0000
        # 这样暂时就可以直接访问该地址

        # 将全局符号boot_stack地址放到栈指针寄存器sp栈顶
        la.global   $sp, {boot_stack}

        # 将立即数boot_stack_size即引导栈的大小加载到临时寄存器t0中
        li.d        $t0, {boot_stack_size}
        # 将t0的值加上sp的栈顶的结果写入栈寄存器
        add.d       $sp, $sp, $t0
        # 从控制状态寄存器 0x20 中读取值到寄存器a0中
        # 该寄存器负责存放CPUID,
        # 即多核处理器的第几个核（看龙架构手册卷一v1.11版）
        # 我们好像只调用了一个核，3A5000有四个核
        # 并且我们没有读这个CPUID,可以先注释掉
        # 经过测试是从0开始的，跟文档描述的一样
        sub.d       $a0, $a0, $a0
        addi.d      $a0, $a0, 0x15
        # 经过上面这样的处理的话，在rust_main的函数签名处添加一个参数如CPUID:usize
        # 就能读取到a0的值为21, 很神奇，蛮好玩的，不过目前来看没什么用

        # 将全局符号entry即程序入口地址rust_main地址放到临时寄存器t0中
        la.global   $t0, {entry}
        # 无条件跳转到t0中的地址开始执行程序，并把结果写入zero寄存器
        # 但是zero寄存器只能为0,相当于平时用Linux把输出丢给/dev/zero
        jirl        $zero,$t0,0x0
        # 因为前面跳转了，所以这行代码实际上应该没什么卵用，
        # 当然也有可能有占位作用，暂时注释掉，没发生什么问题
        # add.d       $sp, $sp, $t0
        ",
        boot_stack_size = const BOOT_STACK_SIZE,
        boot_stack = sym BOOT_STACK,
        entry = sym crate::rust_main
    )
}

pub const BOOT_STACK_SIZE: usize = KERNEL_STACK_SIZE;
#[link_section = ".bss.stack"]
pub(crate) static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];