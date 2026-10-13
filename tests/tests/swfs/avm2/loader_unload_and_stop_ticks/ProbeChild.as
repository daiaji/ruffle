// The loaded movie. Reproduces the game's exact pattern: an `enterFrame`
// listener registered at RUNTIME on its own root, whose handler dereferences
// `MovieClip(root.parent.parent)` - the same first statement as the game's
// `progress_func_days5_sex1_1()`.
//
// Everything is guarded so we can tell *why* it fails rather than just that
// the movie died.

package {
    import flash.display.MovieClip;
    import flash.events.Event;
    import flash.events.MouseEvent;

    public class ProbeChild extends MovieClip {
        public var enterFrameCount:int = 0;
        public var clickCount:int = 0;

        /** Is the parent chain still intact from inside the loaded movie? */
        public var rootNonNull:Boolean = false;
        public var parentNonNull:Boolean = false;
        public var grandNonNull:Boolean = false;

        /** How many times the game's exact statement threw. */
        public var derefErrorCount:int = 0;
        public var lastErrorId:int = 0;

        public var clickListenerRegistered:Boolean = false;
        public var registrationError:String = "";

        public function ProbeChild() {
            super();
            addEventListener(Event.ENTER_FRAME, onEnterFrame);
            stop();
        }

        private function onEnterFrame(event:Event):void {
            if (!clickListenerRegistered) {
                try {
                    MovieClip(root.parent.parent).parentBtn.addEventListener(MouseEvent.CLICK, onClick);
                    clickListenerRegistered = true;
                } catch (e:Error) {
                    registrationError = e.toString();
                }
            }

            // --- exactly what the game does ---
            try {
                rootNonNull = (root != null);
            } catch (e:Error) {
                rootNonNull = false;
            }
            try {
                parentNonNull = (root != null && root.parent != null);
            } catch (e:Error) {
                parentNonNull = false;
            }
            try {
                grandNonNull = (root != null && root.parent != null && root.parent.parent != null);
            } catch (e:Error) {
                grandNonNull = false;
            }
            try {
                // Verbatim the first statement of the game's handler.
                var n:Object = MovieClip(root.parent.parent).flame_num;
            } catch (e:Error) {
                derefErrorCount++;
                lastErrorId = e.errorID;
            }

            enterFrameCount++;
        }

        /** Zero the counters, to measure a fresh window. */
        public function resetCounters():void {
            enterFrameCount = 0;
            derefErrorCount = 0;
            lastErrorId = 0;
        }

        private function onClick(event:MouseEvent):void {
            clickCount++;
        }
    }
}
