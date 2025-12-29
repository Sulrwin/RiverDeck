/**
 * Audio Shim Loader for Linux
 * 
 * This module intercepts require() calls to native audio modules and
 * redirects them to the Linux compatibility shim.
 * 
 * Usage: node --require ./audio-shim-loader.js plugin.js
 */

const Module = require('module');
const path = require('path');
const originalRequire = Module.prototype.require;

// Path to our Linux audio shim
const shimPath = path.join(__dirname, 'linux-audio-shim.js');

console.log('[AudioShimLoader] Audio compatibility layer loaded');
console.log('[AudioShimLoader] Shim path:', shimPath);

// Track all require calls for debugging
let requireCount = 0;

// Override Module.prototype.require
Module.prototype.require = function(id) {
    requireCount++;
    
    // Log ALL requires for now to find the audio module
    console.log(`[AudioShimLoader] Require #${requireCount}: ${id}`);
    
    // Intercept native audio module requires
    if (id.includes('AudioDeviceService') || 
        id.includes('winAudioDeviceService') || 
        id.includes('macAudioDeviceService') ||
        id.includes('linAudioDeviceService') ||
        id.includes('macIntelAudioDeviceService') ||
        (typeof id === 'string' && id.endsWith('.node') && (id.includes('audio') || id.includes('Audio')))) {
        
        console.log('[AudioShimLoader] ***** INTERCEPTED AUDIO MODULE *****');
        console.log('[AudioShimLoader] Original module:', id);
        console.log('[AudioShimLoader] Redirecting to:', shimPath);
        
        return originalRequire.call(this, shimPath);
    }
    
    // Pass through all other requires
    return originalRequire.call(this, id);
};

console.log('[AudioShimLoader] Module intercept active');
console.log('[AudioShimLoader] Monitoring for audio module loads...');

