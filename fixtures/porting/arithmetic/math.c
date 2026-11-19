int clamp(int value, int low, int high) {
    if (value < low) return low;
    if (value > high) return high;
    return value;
}

int magnitude(int value) {
    return value < 0 ? -value : value;
}
