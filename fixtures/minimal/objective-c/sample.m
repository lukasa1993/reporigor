@interface Choice
- (int)choose:(int)a other:(int)b;
@end
@implementation Choice
- (int)choose:(int)a other:(int)b {
  if (a && b) { return 1; }
  return 0;
}
@end
