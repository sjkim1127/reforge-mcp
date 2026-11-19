int global_state = 1;

int read_pointer(int *value) {
    return *value + global_state;
}
