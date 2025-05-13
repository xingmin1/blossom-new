#![no_std]
#![no_main]
// use user_lib::{exit, exec, fork, waitpid, shutdown, sleep};
use user_lib::{exec, exit, fork, shutdown, waitpid, chdir};
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
    let mut exit_code: i32 = 0;
    // xein changed this line to comment
    // sleep(10000);
    let pid = fork();
    if pid == 0 {
        // 只启动bash
        exec(
            path,
            &[path.as_ptr() as *const u8, core::ptr::null()],
            &environ,
        );
        //执行初赛测例

        //===================================MUSL=====================================
        chdir("/musl\0");

        //basic_testcode.sh:
        exec(path, &[path.as_ptr() as *const u8, "-c\0".as_ptr(), "./basic_testcode.sh\0".as_ptr(), core::ptr::null()], &environ);

        //===================================MUSL=====================================

        //********************************************************************************************* 

        //===================================glibc====================================
        chdir("/glibc\0");

        //basic_testcode.sh
        exec(path, &[path.as_ptr() as *const u8, "-c\0".as_ptr(), "./glibc/basic_testcode.sh\0".as_ptr(), core::ptr::null()], &environ);


        //===================================glibc====================================

    } else {
        waitpid(pid as usize, &mut exit_code);
    }
    shutdown();
    0
}
