#include <stdio.h>

#define MAX_ITEMS 10
#define SQUARE(x) ((x) * (x))

typedef unsigned int uint;

struct point {
    int x;
    int y;
};

enum color { RED, GREEN };

static int helper(int a) {
    return a + 1;
}

int add(int a, int b) {
    return a + b;
}

char *name_of(int id) {
    return "x";
}
