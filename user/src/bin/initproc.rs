#![no_std]
#![no_main]
// use user_lib::{exit, exec, fork, waitpid, shutdown, sleep};
use user_lib::{exec, exit, fork, shutdown, waitpid, chdir,wait };
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    exit(main());
}

#[no_mangle]
fn main() -> i32 {
    let path = "/bash\0";
    let environ = [
        "SHELL=/bash\0".as_ptr(),
        "PWD=/\0".as_ptr(),
        "LOGNAME=root\0".as_ptr(),
        "MOTD_SHOWN=pam\0".as_ptr(),
        "HOME=/root\0".as_ptr(),
        "LANG=C.UTF-8\0".as_ptr(),
        "TERM=vt220\0".as_ptr(),
        "USER=root\0".as_ptr(),
        "SHLVL=0\0".as_ptr(),
        "OLDPWD=/root\0".as_ptr(),
        "PS1=\x1b[1m\x1b[32mNPUCore\x1b[0m:\x1b[1m\x1b[34m\\w\x1b[0m\\$ \0".as_ptr(),
        "_=/bin/bash\0".as_ptr(),
        "PATH=/:/bin\0".as_ptr(),
        "LD_LIBRARY_PATH=/\0".as_ptr(),
        core::ptr::null(),
    ];

    let schedule_musl = [
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./basic_testcode.sh\0".as_ptr(),
            core::ptr::null(),
        ],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./busybox_testcode.sh\0".as_ptr(),
            core::ptr::null(),
        ],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./lua_testcode.sh\0".as_ptr(),
            core::ptr::null(),
        ],
        // [
        //     path.as_ptr(),
        //     "-c\0".as_ptr(),
        //     "./lmbench_testcode.sh\0".as_ptr(),
        //     core::ptr::null(),
        // ],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./busybox echo \"#### OS COMP TEST GROUP START lmbench-musl ####\"\0".as_ptr(),
            core::ptr::null(),
        ],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo latency measurements\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 null\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 read\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 write\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox mkdir -p /var/tmp\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox touch /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 stat /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 fstat /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 open /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_select -n 100 -P 1 file\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_sig -P 1 install\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_sig -P 1 catch\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_sig -P 1 prot lat_sig\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_pipe -P 1\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_proc -P 1 fork\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_proc -P 1 exec\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox cp hello /tmp\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_proc -P 1 shell\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lmdd label=\"File /var/tmp/XXX write bandwidth:\" of=/var/tmp/XXX move=1m fsync=1 print=3\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_pagefault -P 1 /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_mmap -P 1 512k /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo file system latency\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_fs /var/tmp\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo Bandwidth measurements\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_pipe -P 1\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_file_rd -P 1 512k io_only /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_file_rd -P 1 512k open2close /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_mmap_rd -P 1 512k mmap_only /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_mmap_rd -P 1 512k open2close /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo context switch overhead\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_ctx -P 1 -s 32 2 4 8 16 24 32 64 96\0".as_ptr(), core::ptr::null()],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./busybox echo \"#### OS COMP TEST GROUP END lmbench-musl ####\"\0".as_ptr(),
            core::ptr::null(),
        ],
        //完成的测试继续添加
    ];


    let schedule_glibc = [
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./basic_testcode.sh\0".as_ptr(),
            core::ptr::null(),
        ],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./busybox_testcode.sh\0".as_ptr(),
            core::ptr::null(),
        ],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./lua_testcode.sh\0".as_ptr(),
            core::ptr::null(),
        ],
        // [
        //     path.as_ptr(),
        //     "-c\0".as_ptr(),
        //     "./lmbench_testcode.sh\0".as_ptr(),
        //     core::ptr::null(),
        // ],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./busybox echo \"#### OS COMP TEST GROUP START lmbench-glibc ####\"\0".as_ptr(),
            core::ptr::null(),
        ],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo latency measurements\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 null\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 read\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 write\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox mkdir -p /var/tmp\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox touch /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 stat /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 fstat /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_syscall -P 1 open /var/tmp/lmbench\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_select -n 100 -P 1 file\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_sig -P 1 install\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_sig -P 1 catch\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_sig -P 1 prot lat_sig\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_pipe -P 1\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_proc -P 1 fork\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_proc -P 1 exec\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox cp hello /tmp\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_proc -P 1 shell\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lmdd label=\"File /var/tmp/XXX write bandwidth:\" of=/var/tmp/XXX move=1m fsync=1 print=3\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_pagefault -P 1 /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_mmap -P 1 512k /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo file system latency\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_fs /var/tmp\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo Bandwidth measurements\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_pipe -P 1\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_file_rd -P 1 512k io_only /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_file_rd -P 1 512k open2close /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_mmap_rd -P 1 512k mmap_only /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        // [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all bw_mmap_rd -P 1 512k open2close /var/tmp/XXX\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./busybox echo context switch overhead\0".as_ptr(), core::ptr::null()],
        [path.as_ptr(), "-c\0".as_ptr(), "./lmbench_all lat_ctx -P 1 -s 32 2 4 8 16 24 32 64 96\0".as_ptr(), core::ptr::null()],
        [
            path.as_ptr(),
            "-c\0".as_ptr(),
            "./busybox echo \"#### OS COMP TEST GROUP END lmbench-glibc ####\"\0".as_ptr(),
            core::ptr::null(),
        ],
        //完成的测试继续添加
    ];

    let mut exit_code: i32 = 0;
    for argv in schedule_musl {
        let pid = fork();
        if pid == 0 {
            chdir("/musl\0");
            exec(path, &argv, &environ);
        } else {
            waitpid(pid as usize, &mut exit_code);
        }
    }

    for argv in schedule_glibc {
        let pid = fork();
        if pid == 0 {
            chdir("/glibc\0");
            exec(path, &argv, &environ);
        } else {
            waitpid(pid as usize, &mut exit_code);
        }
    }
    // for argv in schedule {
    //     let pid = fork();
    //     if pid == 0 {
    //         chdir("/glibc\0");
    //         exec(path, &argv, &environ);
    //     } else {
    //         waitpid(pid as usize, &mut exit_code);
    //     }
    // }
    // for argv in schedule {
    //     let pid = fork();
    //     if pid == 0 {
    //         chdir("/glibc\0");
    //         exec(path, &argv, &environ);
    //     } else {
    //         waitpid(pid as usize, &mut exit_code);
        // let pid = fork();
        // if pid == 0 {
        //     chdir("/glibc\0");
        //     exec(path, &argv, &environ);
        // } else {
        //     waitpid(pid as usize, &mut exit_code);
        // }
    //     }
    // }
    // for argv in schedule {
    //     let pid = fork();
    //     if pid == 0 {
    //         chdir("/glibc\0");
    //         exec(path, &argv, &environ);
    //     } else {
    //         waitpid(pid as usize, &mut exit_code);
    //     }
    // }
     //能进入bash，方便调试
    // if fork() == 0 {
    //     exec(path, &[path.as_ptr() as *const u8, core::ptr::null()], &environ);
    // } else {
    //     loop {
    //         let mut exit_code: i32 = 0;
    //         let pid = wait(&mut exit_code);
    //         // ECHLD is -10
    //         // user_lib::println!(
    //         //     "[initproc] Released a zombie process, pid={}, exit_code={}",
    //         //     pid,
    //         //     exit_code,
    //         // );
    //     }
    // }

    shutdown();
    0
}
