int clamp(int value, int low, int high);
int magnitude(int value);

int score(int value, int bias) {
    int bounded = clamp(value, -100, 100);
    int result = magnitude(bounded) * 2;
    if (bias > 0) result = result + bias;
    else result = result - 1;
    return result;
}
