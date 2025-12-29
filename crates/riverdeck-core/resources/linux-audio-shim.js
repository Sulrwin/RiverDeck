/**
 * Linux Audio Compatibility Shim for Elgato Volume Controller
 * 
 * This module provides a Windows-compatible audio API that redirects
 * calls to the RiverDeck audio router WebSocket server running on port 1844.
 */

const WebSocket = require('ws');

class LinuxAudioDeviceService {
    constructor() {
        this.ws = null;
        this.requestId = 0;
        this.pendingRequests = new Map();
        this.connected = false;
        this.connectPromise = this.connect();
    }

    async connect() {
        return new Promise((resolve, reject) => {
            this.ws = new WebSocket('ws://127.0.0.1:1844');
            
            this.ws.on('open', () => {
                console.log('[AudioShim] Connected to RiverDeck audio router');
                this.connected = true;
                resolve();
            });

            this.ws.on('message', (data) => {
                try {
                    const response = JSON.parse(data.toString());
                    
                    if (response.id !== undefined && this.pendingRequests.has(response.id)) {
                        const { resolve, reject } = this.pendingRequests.get(response.id);
                        this.pendingRequests.delete(response.id);
                        
                        if (response.error) {
                            reject(new Error(response.error.message));
                        } else {
                            resolve(response.result);
                        }
                    }
                } catch (e) {
                    console.error('[AudioShim] Error parsing response:', e);
                }
            });

            this.ws.on('error', (error) => {
                console.error('[AudioShim] WebSocket error:', error);
                reject(error);
            });

            this.ws.on('close', () => {
                console.log('[AudioShim] Disconnected from audio router');
                this.connected = false;
            });
        });
    }

    async sendRequest(method, params = {}) {
        if (!this.connected) {
            await this.connectPromise;
        }

        const id = this.requestId++;
        const request = {
            jsonrpc: '2.0',
            id,
            method,
            params
        };

        return new Promise((resolve, reject) => {
            this.pendingRequests.set(id, { resolve, reject });
            
            try {
                this.ws.send(JSON.stringify(request));
            } catch (e) {
                this.pendingRequests.delete(id);
                reject(e);
            }

            // Timeout after 5 seconds
            setTimeout(() => {
                if (this.pendingRequests.has(id)) {
                    this.pendingRequests.delete(id);
                    reject(new Error('Request timeout'));
                }
            }, 5000);
        });
    }

    // Windows Audio API compatibility methods
    async getDevices() {
        // Return empty array for now - focusing on application audio
        return [];
    }

    async getDefaultDevice() {
        try {
            const device = await this.sendRequest('getSystemDefaultDevice');
            return device;
        } catch (e) {
            console.error('[AudioShim] getDefaultDevice failed:', e);
            return null;
        }
    }

    async getDeviceById(id) {
        // Not implemented yet
        return null;
    }

    // Application audio control methods
    async getApplicationInstanceCount() {
        try {
            const result = await this.sendRequest('getApplicationInstanceCount');
            return result.count;
        } catch (e) {
            console.error('[AudioShim] getApplicationInstanceCount failed:', e);
            return 0;
        }
    }

    async getApplicationInstance(processID) {
        try {
            const result = await this.sendRequest('getApplicationInstance', { processID });
            return result;
        } catch (e) {
            console.error('[AudioShim] getApplicationInstance failed:', e);
            return null;
        }
    }

    async getApplicationInstanceAtIndex(index) {
        try {
            const result = await this.sendRequest('getApplicationInstanceAtIndex', { index });
            return result;
        } catch (e) {
            console.error('[AudioShim] getApplicationInstanceAtIndex failed:', e);
            return null;
        }
    }

    async setApplicationInstanceVolume(processID, volume) {
        try {
            await this.sendRequest('setApplicationInstanceVolume', { processID, volume });
            return true;
        } catch (e) {
            console.error('[AudioShim] setApplicationInstanceVolume failed:', e);
            return false;
        }
    }

    async setApplicationInstanceMute(processID, mute) {
        try {
            await this.sendRequest('setApplicationInstanceMute', { processID, mute });
            return true;
        } catch (e) {
            console.error('[AudioShim] setApplicationInstanceMute failed:', e);
            return false;
        }
    }

    async getApplicationImage(processID, imageSize) {
        // Return empty image data - not critical for functionality
        return null;
    }
}

// Create singleton instance
const audioService = new LinuxAudioDeviceService();

// Export both the class and the singleton
module.exports = audioService;
module.exports.LinuxAudioDeviceService = LinuxAudioDeviceService;

