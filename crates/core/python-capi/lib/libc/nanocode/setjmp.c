#include <stddef.h>
#include <stdint.h>

struct jump {
	void *invocation;
	uint32_t label;
	struct {
		void *env;
		int value;
	} argument;
};

void __wasm_setjmp(void *env, uint32_t label, void *invocation)
{
	struct jump *jump = env;
	if (!label || !invocation) __builtin_trap();
	jump->invocation = invocation;
	jump->label = label;
}

uint32_t __wasm_setjmp_test(void *env, void *invocation)
{
	struct jump *jump = env;
	if (!jump->label || !invocation) __builtin_trap();
	return jump->invocation == invocation ? jump->label : 0;
}

void __wasm_longjmp(void *env, int value)
{
	struct jump *jump = env;
	jump->argument.env = env;
	jump->argument.value = value ? value : 1;
	__builtin_wasm_throw(1, &jump->argument);
}

__asm__(".globl __c_longjmp\n.tagtype __c_longjmp i32\n__c_longjmp:\n");
