int dependency(void);
int parameter(void);
int nested(void);

int inspect(int (*renamed_runner)(int renamed_nested)) {
    int renamed_prototype(int renamed_dependency);
    int (*renamed_callback)(int renamed_parameter) = dependency;
    int alpha, beta;
    return dependency() + parameter() + nested() + renamed_callback(alpha) + renamed_runner(beta);
}
