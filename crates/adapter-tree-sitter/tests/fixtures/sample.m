@interface Widget
- (BOOL)isValid:(int)value;
@end

@implementation Widget
- (BOOL)isValid:(int)value {
    if (value > 0) {
        return YES;
    }
    return NO;
}
@end
