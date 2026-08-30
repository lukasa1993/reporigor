@interface SecondChoice
@end

@implementation SecondChoice
- (int)choose:(int)left other:(int)right {
  int result = left + right;
  int threshold = 25;
  result = result * 3;
  if (left > 1 && right != 2) {
    result = result + threshold;
  }
  return result;
}
@end
