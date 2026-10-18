#include "probe.h"

/* /// 复制调用者拥有的不重叠字节区间；编译时禁用 builtin，避免循环重写为自身调用。 */
void *memcpy(void *destination, const void *source, size_t count)
{
    unsigned char *out = destination;
    const unsigned char *in = source;
    for (size_t i = 0; i < count; ++i)
        out[i] = in[i];
    return destination;
}

/* /// 重叠时从不会覆盖尚未读取源字节的一端开始复制。 */
void *memmove(void *destination, const void *source, size_t count)
{
    unsigned char *out = destination;
    const unsigned char *in = source;
    if ((uintptr_t)out < (uintptr_t)in) {
        for (size_t i = 0; i < count; ++i)
            out[i] = in[i];
    } else {
        while (count) {
            --count;
            out[count] = in[count];
        }
    }
    return destination;
}

/* /// 按无符号字节填充目标，不要求区间对齐。 */
void *memset(void *destination, int byte, size_t count)
{
    unsigned char *out = destination;
    for (size_t i = 0; i < count; ++i)
        out[i] = (unsigned char)byte;
    return destination;
}

/* /// 在首个不同字节处返回无符号差值，零长度不解引用参数。 */
int memcmp(const void *left, const void *right, size_t count)
{
    const unsigned char *a = left;
    const unsigned char *b = right;
    for (size_t i = 0; i < count; ++i) {
        if (a[i] != b[i])
            return (int)a[i] - (int)b[i];
    }
    return 0;
}

/* /// 逐字节读取有效 NUL 终止串，不读取终止符后面的页。 */
size_t strlen(const char *text)
{
    size_t count = 0;
    while (text[count])
        ++count;
    return count;
}
