#include "linux/kernel.h"
#include "linux/init.h"
#include "linux/module.h"

MODULE_DESCRIPTION("My kernel module");
MODULE_AUTHOR("Me");
MODULE_LICENSE("GPL");

static int dummy_init(void)
{
        pr_debug("Hi\n");
        return 0;
}

static void dummy_exit(void)
{
        pr_debug("Bye\n");
}

module_init(dummy_init);
module_exit(dummy_exit);
