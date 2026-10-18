"""根树与制卷入口共用实际输出角色及输入重叠边界。"""

from pathlib import Path
import stat


def output_path(value, *, directory=False):
    """解析普通父目录链接，保留最终目录项并拒绝链接或不匹配的输出角色。"""
    path = Path(value)
    target = path.parent.resolve() / path.name
    try:
        mode = target.lstat().st_mode
    except FileNotFoundError:
        return target
    expected = stat.S_ISDIR if directory else stat.S_ISREG
    if not expected(mode):
        role = "directory" if directory else "regular file"
        raise ValueError(f"output must be an ordinary {role}: {target}")
    return target.resolve()


def reject_overlap(first, second):
    """实际路径不得互相包含；已有普通文件也不得以硬链接共用同一 inode。"""
    if (first == second or first in second.parents or second in first.parents
            or first.exists() and second.exists() and first.samefile(second)):
        raise ValueError(f"input/output paths overlap or alias: {first}, {second}")
    if first.is_dir() and second.is_file() and 1 < second.stat().st_nlink:
        if any(path.is_file() and path.samefile(second) for path in first.rglob("*")):
            raise ValueError(f"output aliases a source file: {second}")
