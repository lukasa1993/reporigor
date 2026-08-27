@interface FirstChoice
- (int)choose:(int)left other:(int)right;
@end

@implementation FirstChoice
- (int)choose:(int)left other:(int)right {
  if (left > 0 && right != 0) {
    return left + right;
  }
  return 0;
}
@end
