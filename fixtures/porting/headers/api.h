#ifndef REFORGE_API_H
#define REFORGE_API_H
#define LOWER (-40)
#define UPPER 40
#define SCALE 3
static inline int bounded(int value) {
    if (value < LOWER) return LOWER;
    if (value > UPPER) return UPPER;
    return value;
}
int scale(int value);
int evaluate(int value);
#endif
