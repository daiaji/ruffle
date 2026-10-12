// Probe: what does `Loader.close()` actually do, in real Flash?
//
// Four scenarios, each on a fresh Loader with per-event counters hooked
// BEFORE anything happens:
//   A. close() with no load ever started on that loader
//   B. load() then close() in the same handler (guaranteed in-flight),
//      then 30 observed frames, then load() again on the same loader
//   C. load(), wait for COMPLETE, then close() after completion
//   D. double close() after completion
//
// The loaded child (child2.swf / TickChild) counts its own enterFrame
// ticks, so if a "cancelled" load actually completes under the hood we
// see it: content becomes non-null, COMPLETE fires, and the child ticks.
//
// Results go to a SharedObject (readable from disk after a real-Flash run)
// and to `trace()` (which is what Ruffle shows us).

package {
    import flash.display.Loader;
    import flash.display.MovieClip;
    import flash.events.Event;
    import flash.events.IOErrorEvent;
    import flash.events.ProgressEvent;
    import flash.net.SharedObject;
    import flash.net.URLRequest;
    import flash.utils.getQualifiedClassName;

    public class CloseProbe extends MovieClip {
        private var loaderA:Loader;
        private var loaderB:Loader;
        private var loaderC:Loader;
        private var loaderD:Loader;
        private var loaderE:Loader;
        private var frame:int = 0;
        private var counts:Object = {};
        private var log:Array = [];

        public function CloseProbe() {
            super();
            addEventListener(Event.ENTER_FRAME, onEnterFrame);
        }

        private function hook(name:String):Loader {
            var l:Loader = new Loader();
            counts[name] = {open:0, init:0, complete:0, ioerror:0, progress:0};
            var c:Object = counts[name];
            l.contentLoaderInfo.addEventListener(Event.OPEN, function(e:Event):void { c.open++; });
            l.contentLoaderInfo.addEventListener(Event.INIT, function(e:Event):void { c.init++; });
            l.contentLoaderInfo.addEventListener(Event.COMPLETE, function(e:Event):void { c.complete++; });
            l.contentLoaderInfo.addEventListener(IOErrorEvent.IO_ERROR, function(e:IOErrorEvent):void { c.ioerror++; });
            l.contentLoaderInfo.addEventListener(ProgressEvent.PROGRESS, function(e:ProgressEvent):void { c.progress++; });
            addChild(l);
            return l;
        }

        private function evs(c:Object):String {
            return "open:" + c.open + " init:" + c.init + " complete:" + c.complete
                + " ioerror:" + c.ioerror + " progress:" + c.progress;
        }

        private function snap(name:String, l:Loader, label:String):void {
            var li:Object = l.contentLoaderInfo;
            var extra:String = "";
            try {
                var mc:MovieClip = l.content as MovieClip;
                if (mc != null) {
                    extra += " childTicked=" + (mc.ticks > 0);
                }
            } catch (err:Error) {
                extra += " childReadErr=" + err.errorID;
            }
            record(label + " [" + name + "] events=" + evs(counts[name])
                + " contentNull=" + (l.content == null)
                + " bytesNull=" + (li.bytes == null)
                + " bytesLoaded=" + li.bytesLoaded
                + " bytesTotal=" + li.bytesTotal + extra);
        }

        private function snapNoChild(name:String, l:Loader, label:String):void {
            // Same as `snap`, but without reading the child's tick count:
            // right after COMPLETE, whether the child has run its first
            // enterFrame yet is Flash's init-event timing, not close()
            // behavior (Ruffle fires the child's enterFrame before
            // init/complete - see the FIXME in Loader load handling).
            var li:Object = l.contentLoaderInfo;
            record(label + " [" + name + "] events=" + evs(counts[name])
                + " contentNull=" + (l.content == null)
                + " bytesNull=" + (li.bytes == null)
                + " bytesLoaded=" + li.bytesLoaded
                + " bytesTotal=" + li.bytesTotal);
        }

        private function onEnterFrame(e:Event):void {
            frame++;

            if (frame == 1) {
                record("=== A: close() with no load in progress ===");
                loaderA = hook("A");
                try {
                    loaderA.close();
                    record("A close(): no throw");
                } catch (err:Error) {
                    record("A close() THREW: id=" + err.errorID + " cls=" + getQualifiedClassName(err));
                }
                snap("A", loaderA, "A right after");
            } else if (frame == 3) {
                snap("A", loaderA, "A 2 frames later");

                record("=== B: close() on an in-flight load ===");
                loaderB = hook("B");
                loaderB.load(new URLRequest("child2.swf"));
                try {
                    loaderB.close();
                    record("B close(): no throw");
                } catch (err:Error) {
                    record("B close() THREW: id=" + err.errorID + " cls=" + getQualifiedClassName(err));
                }
                snap("B", loaderB, "B same handler");
            } else if (frame == 33) {
                snap("B", loaderB, "B 30 frames later");

                record("=== B2: load() again on the same closed loader ===");
                loaderB.load(new URLRequest("child2.swf"));
                snap("B", loaderB, "B right after reload");
            } else if (frame == 63) {
                snap("B", loaderB, "B 30 frames after reload");

                record("=== C: close() after COMPLETE ===");
                loaderC = hook("C");
                loaderC.load(new URLRequest("child2.swf"));
            } else if (frame == 67) {
                snap("C", loaderC, "C before close (4 frames in)");
                try {
                    loaderC.close();
                    record("C close(): no throw");
                } catch (err:Error) {
                    record("C close() THREW: id=" + err.errorID + " cls=" + getQualifiedClassName(err));
                }
                snap("C", loaderC, "C after close");
            } else if (frame == 71) {
                snap("C", loaderC, "C 4 frames later");

                record("=== D: double close after COMPLETE ===");
                loaderD = hook("D");
                loaderD.load(new URLRequest("child2.swf"));
            } else if (frame == 75) {
                try {
                    loaderD.close();
                    loaderD.close();
                    record("D double close(): no throw");
                } catch (err:Error) {
                    record("D double close() THREW: id=" + err.errorID + " cls=" + getQualifiedClassName(err));
                }
                snap("D", loaderD, "D after double close");

                record("=== E: close() inside the COMPLETE handler ===");
                loaderE = hook("E");
                var self:CloseProbe = this;
                loaderE.contentLoaderInfo.addEventListener(Event.COMPLETE, function(e:Event):void {
                    try {
                        self.loaderE.close();
                        self.record("E close() in COMPLETE handler: no throw");
                    } catch (err:Error) {
                        self.record("E close() in COMPLETE handler THREW: id=" + err.errorID + " cls=" + getQualifiedClassName(err));
                    }
                    self.snapNoChild("E", self.loaderE, "E inside handler");
                });
                loaderE.load(new URLRequest("child2.swf"));
            } else if (frame == 79) {
                snap("E", loaderE, "E 4 frames later");
                finish();
            }
        }

        private function record(line:String):void {
            log.push(line);
            trace(line);
        }

        /** Persist everything, then stop ticking so the projector can be closed. */
        private function finish():void {
            trace("PROBE_DONE");
            try {
                var so:SharedObject = SharedObject.getLocal("ZZPROBE6");
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
