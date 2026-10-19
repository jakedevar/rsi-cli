// Installs the Enter/Space activation guard (modal-gate.ts activationGuard) on
// window in the capture phase. main.ts imports this module FIRST, so ES module
// evaluation order registers the guard before any other module's listener
// (hub #3128): no later keydown listener can act on a focused control's keys.
import { installActivationGuard } from './modal-gate.js';

installActivationGuard(window);
