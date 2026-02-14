#ifndef MODULE_INIT_H
#define MODULE_INIT_H

/* Standard types for init/exit callbacks */
typedef int  (*initcall_t)(void);
typedef void (*exitcall_t)(void);

/* Small portability helpers */
#define __stringify_1(x) #x
#define __stringify(x)  __stringify_1(x)

#define __concat_1(a,b) a##b
#define __concat(a,b)   __concat_1(a,b)

/* Unique identifier generator: prefer __COUNTER__ if available, fallback to __LINE__ */
#if defined(__COUNTER__)
# define __unique_id(prefix) __concat(prefix, __COUNTER__)
#else
# define __unique_id(prefix) __concat(prefix, __LINE__)
#endif

/* Mark symbol as used so compiler won't drop it */
#if defined(__GNUC__) || defined(__clang__)
# define __used      __attribute__((used))
#else
# define __used
#endif

/* Public macros that mirror Linux-style declaration chain */
#define module_init(x)   __initcall(x)
#define module_exit(x)   __exitcall(x)

#define __initcall(fn)           device_initcall(fn)
#define device_initcall(fn)      __define_initcall(fn, 6)

#define __exitcall(fn)           device_exitcall(fn)
#define device_exitcall(fn)      __define_exitcall(fn, 6)

/* Final helpers: create a unique static symbol placed into the named section. */
#define __define_initcall(fn, id) \
        __unique_define_initcall((fn), (id), ".initcall" __stringify(id) ".init")

#define __define_exitcall(fn, id) \
        __unique_define_exitcall((fn), (id), ".exitcall" __stringify(id) ".exit")

/* Define the actual variables placed in section 'sec' */
#define __unique_define_initcall(fn, id, sec)                          \
    static initcall_t __unique_id(__initcall_) __used                   \
        __attribute__((section(sec))) = (initcall_t)(fn)

#define __unique_define_exitcall(fn, id, sec)                          \
    static exitcall_t __unique_id(__exitcall_) __used                   \
        __attribute__((section(sec))) = (exitcall_t)(fn)

#endif


#define MODULE_LICENSE(x)
#define MODULE_AUTHOR(x)
#define MODULE_DESCRIPTION(x)
