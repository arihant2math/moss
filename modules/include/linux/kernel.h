extern void printk(const char *fmt, ...);

#define pr_debug(fmt, ...) printk(fmt, ##__VA_ARGS__)
