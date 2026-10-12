// Probe: `Loader.unloadAndStop()` called from the FRAME-SCRIPT phase.
//
// The other probe (`loader_unload_and_stop_ticks`) calls it from an
// `enterFrame` handler, which is how it was first measured: real Flash then
// gives the unloaded content exactly ONE more frame tick - the tick it was
// in the middle of when the call happened.
//
// The game, however, calls `unloadAndStop()` from a frame script
// (`ブラックアウト_21.frame21`). Flash's per-frame order is
// `enterFrame -> frameConstructed -> frame scripts -> exitFrame -> render`,
// so from a frame script the content's enterFrame for the current frame has
// already run. `Event.FRAME_CONSTRUCTED` fires right before frame scripts,
// so it reproduces the game's call site.
//
// Two hypotheses distinguish two Flash behaviors:
//   A. "stops at the end of the current frame's tick" -> 0 further ticks
//   B. "gives one more enterFrame after the call"     -> 1 further tick
//
// Results go to a SharedObject (readable from disk after a real-Flash run)
// and to `trace()` (which is what Ruffle shows us).

package {
    import flash.display.Loader;
    import flash.display.MovieClip;
    import flash.events.Event;
    import flash.net.SharedObject;
    import flash.net.URLRequest;

    public class ProbeFS extends MovieClip {
        private var loader:Loader = new Loader();
        private var child:MovieClip;
        private var frame:int = 0;
        private var ticksAtUnload:int = -1;
        private var log:Array = [];

        public function ProbeFS() {
            super();
            addEventListener(Event.ENTER_FRAME, onEnterFrame);
            addEventListener(Event.FRAME_CONSTRUCTED, onFrameConstructed);
            loader.contentLoaderInfo.addEventListener(Event.COMPLETE, onComplete);
            addChild(loader);

            try {
                loader.load(new URLRequest("child2.swf"));
            } catch (e:Error) {
                record("EXCEPTION loading child: " + e);
                finish();
            }
        }

        private function onComplete(e:Event):void {
            child = e.target.content as MovieClip;
        }

        private function onEnterFrame(e:Event):void {
            frame++;

            if (frame == 40) {
                record("--- window 1: the 37 frames after the unload ---");
                record("ticks the unloaded movie got     = " + (child.ticks - ticksAtUnload));
                record("enterFrame listener after        = " + child.hasEventListener(Event.ENTER_FRAME));
            } else if (frame == 60) {
                record("--- window 2: 20 frames much later ---");
                record("ticks the unloaded movie got     = " + (child.ticks - ticksAtUnload));
                finish();
            }
        }

        private function onFrameConstructed(e:Event):void {
            if (frame < 3 || child == null || ticksAtUnload != -1) {
                return;
            }
            record("--- unloadAndStop() from the frame-script phase ---");
            ticksAtUnload = child.ticks;
            record("child enterFrame listener before = " + child.hasEventListener(Event.ENTER_FRAME));

            loader.unloadAndStop();

            record("child enterFrame listener after  = " + child.hasEventListener(Event.ENTER_FRAME));
        }

        private function record(line:String):void {
            log.push(line);
            trace(line);
        }

        /** Persist everything, then stop ticking so the projector can be closed. */
        private function finish():void {
            trace("PROBE_DONE");
            try {
                var so:SharedObject = SharedObject.getLocal("ZZPROBE7");
                so.data.lines = log.join("\n");
                so.data.count = log.length;
                so.flush();
                trace("PROBE_SOL_WRITTEN");
            } catch (e:Error) {
                trace("PROBE_SOL_FAILED: " + e);
            }
            removeEventListener(Event.ENTER_FRAME, onEnterFrame);
            removeEventListener(Event.FRAME_CONSTRUCTED, onFrameConstructed);
        }
    }
}
