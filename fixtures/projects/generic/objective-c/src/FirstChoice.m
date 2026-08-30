@interface FirstChoice
@end

@implementation FirstChoice
- (int)choose:(int)left other:(int)right {
  int total = left + right;
  int limit = 10;
  total = total * 2;
  if (left > 0 && right != 0) {
    total = total + limit;
  }
  return total;
}
@end
