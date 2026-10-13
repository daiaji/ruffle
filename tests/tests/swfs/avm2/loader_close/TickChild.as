// Minimal loadable child: counts its own enterFrame ticks. The parent
// reads `ticks` off `loader.content` when the content is accessible, so a
// "cancelled" load that actually completed under the hood is visible.
//
// It traces nothing (the parent's `record()` lines are what the test
// compares). On its first tick it dereferences a property that does not
// exist on the parent (a sealed class) and records the outcome in a
// SharedObject: Flash returns `undefined` there, while Ruffle throws
// `Error #1069` - a separate, known divergence from the unloadAndStop probe.

package {
    import flash.display.MovieClip;
    import flash.events.Event;
    import flash.net.SharedObject;

    public class TickChild extends MovieClip {
        public var ticks:int = 0;

        public function TickChild() {
            super();
            addEventListener(Event.ENTER_FRAME, onTick);
        }

        private function onTick(e:Event):void {
            ticks++;
            if (ticks == 1) {
                var result:String;
                try {
                    var v:* = MovieClip(root.parent.parent).no_such_prop;
                    result = "ok value=" + v;
                } catch (err:Error) {
                    result = "THREW id=" + err.errorID;
                }
                try {
                    var so:SharedObject = SharedObject.getLocal("ZZTICK6");
                    so.data.result = result;
                    so.flush();
                } catch (e2:Error) {
                    // Ignore - this is probe-side evidence only.
                }
            }
        }
    }
}
