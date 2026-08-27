@interface SecondChoice
- (int)choose:(int)left other:(int)right;
@end

@implementation SecondChoice
- (int)choose:(int)left other:(int)right {
  if (left > 0 && right != 0) {
    return left + right;
  }
  return 0;
}
@end
