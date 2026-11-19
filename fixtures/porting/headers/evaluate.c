#include "api.h"
#include <limits.h>
#if INT_MAX == 2147483647
int evaluate(int value) { return scale(value) + bounded(value); }
#else
#error unsupported int width
#endif
