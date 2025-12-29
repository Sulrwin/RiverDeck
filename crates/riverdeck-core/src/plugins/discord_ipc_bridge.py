#!/usr/bin/env python3
"""
Discord IPC Bridge for Wine

This script creates a bridge between Wine's named pipe system and Linux's
Unix domain sockets for Discord IPC communication.

On Windows, Discord uses named pipes (\\.\pipe\discord-ipc-0).
On Linux, Discord uses Unix domain sockets (/run/user/$UID/discord-ipc-0).

This bridge allows Windows executables running under Wine to communicate
with the native Linux Discord client.
"""

import os
import socket
import struct
import json
import threading
import time
import logging
from pathlib import Path

logging.basicConfig(level=logging.INFO, format='[DiscordIPCBridge] %(message)s')
logger = logging.getLogger('DiscordIPCBridge')


class DiscordIPCBridge:
    def __init__(self, ipc_num=0):
        self.ipc_num = ipc_num
        self.running = False
        
        # Linux Discord IPC socket path
        uid = os.getuid()
        self.linux_socket_path = f"/run/user/{uid}/discord-ipc-{ipc_num}"
        
        # Wine named pipe path (simulated via Unix socket in Wine prefix)
        # Wine translates \\.\pipe\name to $WINEPREFIX/dosdevices/com{X}
        # We'll create a Unix socket that Wine can connect to
        wine_prefix = os.environ.get('WINEPREFIX', os.path.expanduser('~/.wine'))
        self.wine_socket_dir = Path(wine_prefix) / 'drive_c' / 'temp'
        self.wine_socket_dir.mkdir(parents=True, exist_ok=True)
        self.wine_socket_path = self.wine_socket_dir / f'discord-ipc-{ipc_num}.sock'
        
        logger.info(f"Linux Discord socket: {self.linux_socket_path}")
        logger.info(f"Wine bridge socket: {self.wine_socket_path}")

    def connect_to_discord(self):
        """Connect to the native Linux Discord client"""
        if not os.path.exists(self.linux_socket_path):
            logger.error(f"Discord IPC socket not found: {self.linux_socket_path}")
            logger.error("Is Discord running on Linux?")
            return None
        
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(self.linux_socket_path)
            logger.info("Connected to Discord")
            return sock
        except Exception as e:
            logger.error(f"Failed to connect to Discord: {e}")
            return None

    def start_bridge_server(self):
        """Start a Unix socket server that Wine can connect to"""
        # Remove old socket if it exists
        if self.wine_socket_path.exists():
            self.wine_socket_path.unlink()
        
        server_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server_sock.bind(str(self.wine_socket_path))
        server_sock.listen(1)
        
        logger.info(f"Bridge server listening on {self.wine_socket_path}")
        logger.info("Waiting for Wine plugin to connect...")
        
        self.running = True
        
        while self.running:
            try:
                server_sock.settimeout(1.0)
                wine_conn, _ = server_sock.accept()
                logger.info("Wine plugin connected")
                
                discord_conn = self.connect_to_discord()
                if discord_conn:
                    self.bridge_connections(wine_conn, discord_conn)
                else:
                    wine_conn.close()
                    logger.error("Could not establish Discord connection")
            except socket.timeout:
                continue
            except Exception as e:
                logger.error(f"Server error: {e}")
                break
        
        server_sock.close()
        if self.wine_socket_path.exists():
            self.wine_socket_path.unlink()

    def bridge_connections(self, wine_conn, discord_conn):
        """Bridge data between Wine and Discord connections"""
        logger.info("Bridging connections...")
        
        def forward(src, dst, name):
            try:
                while self.running:
                    data = src.recv(4096)
                    if not data:
                        logger.info(f"{name} connection closed")
                        break
                    dst.sendall(data)
            except Exception as e:
                logger.error(f"{name} forward error: {e}")
            finally:
                src.close()
                dst.close()
        
        # Create two threads for bidirectional forwarding
        wine_to_discord = threading.Thread(
            target=forward,
            args=(wine_conn, discord_conn, "Wine->Discord"),
            daemon=True
        )
        discord_to_wine = threading.Thread(
            target=forward,
            args=(discord_conn, wine_conn, "Discord->Wine"),
            daemon=True
        )
        
        wine_to_discord.start()
        discord_to_wine.start()
        
        # Wait for both threads to complete
        wine_to_discord.join()
        discord_to_wine.join()
        
        logger.info("Bridge connections closed")

    def run(self):
        """Run the bridge"""
        try:
            self.start_bridge_server()
        except KeyboardInterrupt:
            logger.info("Shutting down...")
            self.running = False
        except Exception as e:
            logger.error(f"Fatal error: {e}")


def main():
    bridge = DiscordIPCBridge(ipc_num=0)
    bridge.run()


if __name__ == "__main__":
    main()

