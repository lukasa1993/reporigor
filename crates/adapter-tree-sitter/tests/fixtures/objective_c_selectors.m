@interface Controller
- (void)reset;
- (void)setValue:(int)value forKey:(id)key;
@end

@implementation Controller
- (void)reset {}
- (void)setValue:(int)value forKey:(id)key {
    (void)value;
    (void)key;
}
@end
