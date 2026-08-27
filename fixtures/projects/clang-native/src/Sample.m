@interface Sample
- (int)classify:(int)left other:(int)right;
@end

@implementation Sample
- (int)classify:(int)left other:(int)right {
  if (left > 0 && right > 0) {
    return left + right;
  }
  return 0;
}
@end
