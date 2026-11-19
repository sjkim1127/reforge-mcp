#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    char *end; errno = 0;
    long value = strtol(argv[1], &end, 10);
    if (end == argv[1] || *end != '\0') puts("{\"error\":\"invalid\"}");
    else if (errno == ERANGE || value < INT_MIN || value > INT_MAX) puts("{\"error\":\"overflow\"}");
    else printf("{\"value\":%ld}\n", value);
    return 0;
}
