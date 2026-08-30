int dependency(void);
int parameter(void);
int nested(void);

int inspect(int (*runner)(int nested)) {
    int prototype(int dependency);
    int (*callback)(int parameter) = dependency;
    int first, second;
    return dependency() + parameter() + nested() + callback(first) + runner(second);
}
