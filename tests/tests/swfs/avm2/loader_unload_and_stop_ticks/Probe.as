// Probe: what does `Loader.unloadAndStop()` actually do, in real Flash?
//
// The child registers both of its listeners at RUNTIME:
//   1. `enterFrame` on the loaded movie's own root
//   2. `click` on an object owned by the PARENT movie (`parentBtn`) - the same
//      shape as the game's `skip_btn`.
//
// Its `enterFrame` handler then does what the game's `progress_func_days5_*`
// does: dereference `MovieClip(root.parent.parent)`.
//
// Everything is measured in two windows after the unload, so a movie that ticks
// once and stops is distinguishable from one that keeps ticking forever.
//
// Results go to a SharedObject (readable from disk after a real-Flash run) and
// to `trace()` (which is what Ruffle shows us).

package {
    import flash.display.Loader;
    import flash.display.MovieClip;
    import flash.display.Sprite;
    import flash.events.Event;
    import flash.events.MouseEvent;
    import flash.net.SharedObject;
    import flash.net.URLRequest;

    public dynamic class Probe extends MovieClip {
        /** The child registers a CLICK listener on this - it belongs to us. */
        public var parentBtn:Sprite;

        private var loader:Loader = new Loader();
        private var child:MovieClip;
        private var frame:int = 0;
        private var ticksAtUnload:int = 0;
        private var log:Array = [];

        public function Probe() {
            super();
            parentBtn = new Sprite();
            parentBtn.name = "parentBtn";
            addChild(parentBtn);

            addEventListener(Event.ENTER_FRAME, onEnterFrame);
            loader.contentLoaderInfo.addEventListener(Event.COMPLETE, onComplete);
            addChild(loader);

            try {
                loader.load(new URLRequest("child.swf"));
            } catch (e:Error) {
                record("EXCEPTION loading child: " + e);
                finish();
            }
        }

        private function onComplete(e:Event):void {
            child = e.target.content as MovieClip;
        }

        private function onEnterFrame(e:Event):void {
            if (child == null) {
                return;
            }
            frame++;

            if (frame == 3) {
                ticksAtUnload = child.enterFrameCount;
                record("child clickListenerRegistered    = " + child.clickListenerRegistered
                    + (child.registrationError == "" ? "" : " (error: " + child.registrationError + ")"));
                record("child enterFrame listener before = " + child.hasEventListener(Event.ENTER_FRAME));
                record("parentBtn CLICK listener before  = " + parentBtn.hasEventListener(MouseEvent.CLICK));

                record("--- unloadAndStop() + removeChild(loader) ---");
                loader.unloadAndStop();
                removeChild(loader);

                record("child enterFrame listener after  = " + child.hasEventListener(Event.ENTER_FRAME));
                record("parentBtn CLICK listener after   = " + parentBtn.hasEventListener(MouseEvent.CLICK));
                dispatchClick("#1");
            } else if (frame == 40) {
                record("--- window 1: the 37 frames after the unload ---");
                record("ticks the unloaded movie got     = " + (child.enterFrameCount - ticksAtUnload));
                record("times the game's statement threw = " + child.derefErrorCount);
                record("last error id                    = " + child.lastErrorId);
                record("root non-null                    = " + child.rootNonNull);
                record("root.parent non-null             = " + child.parentNonNull);
                record("root.parent.parent non-null      = " + child.grandNonNull);
                child.resetCounters();
            } else if (frame == 60) {
                record("--- window 2: 20 frames much later ---");
                record("ticks the unloaded movie got     = " + child.enterFrameCount);
                record("times the game's statement threw = " + child.derefErrorCount);
                dispatchClick("#2");
                finish();
            }
        }

        private function dispatchClick(which:String):void {
            try {
                parentBtn.dispatchEvent(new MouseEvent(MouseEvent.CLICK));
            } catch (err:Error) {
                record("EXCEPTION dispatching click " + which + ": " + err);
            }
            record("child click handler ran (dispatch " + which + ") = " + (child.clickCount > 0));
        }

        private function record(line:String):void {
            log.push(line);
            trace(line);
        }

        /** Persist everything, then stop ticking so the projector can be closed. */
        private function finish():void {
            trace("PROBE_DONE");
            try {
                var so:SharedObject = SharedObject.getLocal("ZZPROBE5");
                so.data.lines = log.join("\n");
                so.data.count = log.length;
                so.flush();
                trace("PROBE_SOL_WRITTEN");
            } catch (e:Error) {
                trace("PROBE_SOL_FAILED: " + e);
            }
            removeEventListener(Event.ENTER_FRAME, onEnterFrame);
        }
    }
}
