/**
 * HAPI MCP server
 * Provides HAPI CLI specific tools including chat session title management
 */

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { createServer, type IncomingMessage } from "node:http";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { AddressInfo } from "node:net";
import { z } from "zod";
import { logger } from "@/ui/logger";
import { ApiSessionClient } from "@/api/apiSession";
import { randomUUID } from "node:crypto";
import {
    detectDisplayMediaMimeType,
    detectImageMimeType,
    detectVideoMimeType,
    readBoundedRegularFile,
    registerGeneratedImage,
} from "@/modules/common/generatedImages";
import type { InlineMediaSource } from "@/modules/common/inlineMediaSource";
import { DISPLAY_IMAGE_TOOL_DESCRIPTION, DISPLAY_MEDIA_TOOL_DESCRIPTION, DISPLAY_VIDEO_TOOL_DESCRIPTION } from "@/modules/common/displayToolDescriptions";
import { resolveSkill } from "@/modules/common/skills";
import { toSessionSummaryMetadata } from '@hapi/protocol'
import { isPeerToolsDisabled } from "@/modules/common/peerTools";
import { handlePeerTool, peerTools } from "@/modules/pingPeer/peerTool";
import { applySessionDisplayRename, normalizeSessionDisplayTitle } from "@/agent/sessionDisplayRename";
import {
    SESSION_JOB_TOOL_DESCRIPTION,
    SESSION_JOB_TOOL_NAME,
    handleSessionJobTool,
    sessionJobInputSchema,
    type SessionJobToolArgs,
} from "@/modules/sessionJob/sessionJobMcp";

export type HapiMcpToolName =
    | 'change_title'
    | 'display_image'
    | 'display_video'
    | 'display_media'
    | 'list_peers'
    | 'ping_peer'
    | 'inspect_peer'
    | 'spawn_peer'
    | 'session_job'
    | 'skill_lookup';

type StartHappyServerOptions = {
    /**
     * When true (default), change_title writes metadata.name (web rename
     * semantics). Set false for Codex collab so child agents do not rename
     * the parent HAPI session; the parent launcher applies the title later.
     */
    emitTitleSummary?: boolean;
    enableChangeTitle?: boolean;
    onChangeTitle?: (title: string) => void;
    /**
     * Session project cwd for resolving relative `spawn_peer` directories.
     * Prefer the launcher's effective cwd (including Codex `--cd` overrides).
     * Falls back to `skillLookup.workingDirectory`, then hub metadata.path.
     */
    workingDirectory?: string;
    skillLookup?: {
        workingDirectory: string;
        flavor: string;
    };
    /**
     * Restrict the server to an explicit subset of HAPI tools. Omit to keep
     * the historical behavior where launchers choose through the other flags.
     */
    enabledTools?: readonly HapiMcpToolName[];
};

/**
 * Resolve the base cwd for MCP spawn_peer relative directories.
 * Prefer launcher workingDirectory (Codex --cd), then skillLookup, then
 * session metadata.path so bridges without skillLookup still anchor to the
 * session tree instead of the long-lived HAPI process cwd.
 */
export function resolveMcpSpawnPeerCwd(options: {
    workingDirectory?: string | null
    skillWorkingDirectory?: string | null
    sessionPath?: string | null
}): string | undefined {
    const fromLauncher = (options.workingDirectory ?? '').trim()
    if (fromLauncher) return fromLauncher
    const fromSkill = (options.skillWorkingDirectory ?? '').trim()
    if (fromSkill) return fromSkill
    const fromSession = (options.sessionPath ?? '').trim()
    return fromSession || undefined
}

/** Registered on the MCP server, but never pre-approved via Claude --allowedTools. */
const CLAUDE_MANUAL_APPROVAL_HAPI_TOOLS = new Set([
    'display_media',
    'display_video',
    'ping_peer',
    'inspect_peer',
    'spawn_peer'
]);

/**
 * Map HAPI MCP tool names to Claude `--allowedTools` entries.
 * Keeps `display_media` / `display_video` (arbitrary local-path readers), `ping_peer`,
 * `inspect_peer`, and `spawn_peer` off the auto-allow list so they still prompt.
 * `list_peers` stays allowed (discovery shortlist only).
 */
export function toClaudeAllowedHapiMcpTools(toolNames: string[]): string[] {
    return toolNames
        .filter((toolName) => !CLAUDE_MANUAL_APPROVAL_HAPI_TOOLS.has(toolName))
        .map((toolName) => `mcp__hapi__${toolName}`);
}

function createHapiMcpServer(
    client: ApiSessionClient,
    emitTitleSummary: boolean,
    enableChangeTitle: boolean,
    skillLookup: StartHappyServerOptions['skillLookup'],
    workingDirectory: string | undefined,
    enabledTools: ReadonlySet<HapiMcpToolName>,
    onChangeTitle?: StartHappyServerOptions['onChangeTitle']
): McpServer {
    const registerTool = (name: HapiMcpToolName, config: any, handler: any): void => {
        if (enabledTools.has(name)) {
            mcp.registerTool<any, any>(name, config, handler);
        }
    };
    const handler = async (title: string) => {
        logger.debug('[hapiMCP] Changing title to:', title);
        try {
            if (!normalizeSessionDisplayTitle(title)) return { success: false, error: 'Title must not be empty' };
            if (!await client.refreshMetadata()) {
                return { success: false, error: 'Could not read the current title lock from the Hub.' };
            }
            if (client.getMetadata()?.titleLocked) {
                return { success: false, error: 'Session title is locked by the user. Only the user can unlock it.' };
            }
            if (!emitTitleSummary) {
                // Codex collab: acknowledge the tool call without writing.
                // Parent-thread mcp_tool_call_end applies the rename later.
                return { success: true };
            }
            if (!applySessionDisplayRename(client, title)) {
                return { success: false, error: 'Title must not be empty' };
            }
            if (!await client.flushMetadata()) {
                return { success: false, error: 'Title update could not be confirmed by the Hub.' };
            }
            // The user may lock the title while this write is in flight.
            // The ACK carries the Hub's preserved title and lock.
            if (client.getMetadata()?.titleLocked) {
                return { success: false, error: 'Session title is locked by the user. Only the user can unlock it.' };
            }
            if (onChangeTitle) {
                onChangeTitle(normalizeSessionDisplayTitle(title)!);
                if (!await client.flushMetadata()) {
                    return { success: false, error: 'Title update could not be confirmed by the Hub.' };
                }
                if (client.getMetadata()?.titleLocked) {
                    return { success: false, error: 'Session title is locked by the user. Only the user can unlock it.' };
                }
            }
            return { success: true };
        } catch (error) {
            return { success: false, error: String(error) };
        }
    };

    const mcp = new McpServer({
        name: "HAPI MCP",
        version: "1.0.0",
    });

    const changeTitleInputSchema: z.ZodTypeAny = z.object({
        title: z.string().describe('The new title for the chat session'),
    });

    const displayImageInputSchema: z.ZodTypeAny = z.object({
        path: z.string().describe('Absolute filesystem path of the local image to display to the human user. This file is sent for user display, not provided to the model for image inspection'),
        title: z.string().optional().describe('Optional display title or filename shown to the human user'),
    });

    const skillLookupInputSchema: z.ZodTypeAny = z.object({
        name: z.string().trim().min(1).max(128).describe('Exact skill name shown by HAPI skill autocomplete'),
    });

    const displayVideoInputSchema: z.ZodTypeAny = z.object({
        path: z.string().describe('Local filesystem path of the video to display inline (mp4 or webm)'),
        title: z.string().optional().describe('Optional display title or filename for the video'),
    });

    const displayMediaInputSchema: z.ZodTypeAny = z.object({
        path: z.string().describe('Local filesystem path of the media or file to send to the user'),
        title: z.string().trim().min(1).max(255).optional().describe('Optional display title or filename'),
    });

    const maxInlineMediaBytes = 25 * 1024 * 1024;

    // HAPI_DISABLE_PEER_TOOLS=1: peer tools are neither registered nor
    // advertised, so wrapped agents cannot browse or message other sessions.
    const peerToolsDisabled = isPeerToolsDisabled();



    async function displayInlineMedia(
        args: { path: string; title?: string },
        mediaKind: 'image' | 'video' | 'media',
        toolName: 'display_image' | 'display_video' | 'display_media'
    ) {
        const bytes = await readBoundedRegularFile(args.path, maxInlineMediaBytes);
        const mimeType = mediaKind === 'video'
            ? detectVideoMimeType(bytes)
            : mediaKind === 'image'
                ? detectImageMimeType(bytes)
                : detectDisplayMediaMimeType(bytes);
        if (!mimeType) {
            throw new Error(mediaKind === 'video' ? 'Unsupported video content' : 'Unsupported image content');
        }

        const media = registerGeneratedImage({
            id: randomUUID(),
            path: args.path,
            fileName: args.title,
            mimeType,
            bytes
        });

        const source: InlineMediaSource = {
            ingress: 'mcp',
            toolName,
        };

        client.sendAgentMessage({
            type: 'generated-image',
            imageId: media.id,
            fileName: media.fileName,
            mimeType: media.mimeType,
            id: randomUUID(),
            source,
        });

        return media;
    }
    if (enableChangeTitle) {
        registerTool('change_title', {
            description: 'Change the title of the current HAPI chat session. Call once when the user\'s primary objective is clear; use a concise task title.',
            title: 'Change Chat Title',
            inputSchema: changeTitleInputSchema,
        }, async (args: { title: string }) => {
            const response = await handler(args.title);
            logger.debug('[hapiMCP] Response:', response);
            const displayTitle = normalizeSessionDisplayTitle(args.title) ?? args.title;

            if (response.success) {
                return {
                    content: [
                        {
                            type: 'text' as const,
                            text: `Successfully changed chat title to: "${displayTitle}"`,
                        },
                    ],
                    isError: false,
                };
            }

            return {
                content: [
                    {
                        type: 'text' as const,
                        text: `Failed to change chat title: ${response.error || 'Unknown error'}`,
                    },
                ],
                isError: true,
            };
        });
    }

    registerTool('display_image', {
        description: DISPLAY_IMAGE_TOOL_DESCRIPTION,
        title: 'Display Image',
        inputSchema: displayImageInputSchema,
    }, async (args: { path: string; title?: string }) => {
        logger.debug('[hapiMCP] Display image:', args.path);

        try {
            const image = await displayInlineMedia(args, 'image', 'display_image');

            return {
                content: [
                    {
                        type: 'text' as const,
                        text: `Displayed image: ${image.fileName}`,
                    },
                ],
                isError: false,
            };
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            logger.debug('[hapiMCP] Failed to display image:', message);
            return {
                content: [
                    {
                        type: 'text' as const,
                        text: `Failed to display image: ${message}`,
                    },
                ],
                isError: true,
            };
        }
    });

    registerTool('display_video', {
        description: DISPLAY_VIDEO_TOOL_DESCRIPTION,
        title: 'Display Video',
        inputSchema: displayVideoInputSchema,
    }, async (args: { path: string; title?: string }) => {
        logger.debug('[hapiMCP] Display video:', args.path);

        try {
            const video = await displayInlineMedia(args, 'video', 'display_video');

            return {
                content: [
                    {
                        type: 'text' as const,
                        text: `Displayed video: ${video.fileName}`,
                    },
                ],
                isError: false,
            };
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            logger.debug('[hapiMCP] Failed to display video:', message);
            return {
                content: [
                    {
                        type: 'text' as const,
                        text: `Failed to display video: ${message}`,
                    },
                ],
                isError: true,
            };
        }
    });

    registerTool('display_media', {
        description: DISPLAY_MEDIA_TOOL_DESCRIPTION,
        title: 'Display Media',
        inputSchema: displayMediaInputSchema,
    }, async (args: { path: string; title?: string }) => {
        logger.debug('[hapiMCP] Display media:', args.path);

        try {
            const media = await displayInlineMedia(args, 'media', 'display_media');
            return {
                content: [{ type: 'text' as const, text: `Displayed media: ${media.fileName}` }],
                isError: false,
            };
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            logger.debug('[hapiMCP] Failed to display media:', message);
            return {
                content: [{ type: 'text' as const, text: `Failed to display media: ${message}` }],
                isError: true,
            };
        }
    });

    registerTool(SESSION_JOB_TOOL_NAME, {
        description: SESSION_JOB_TOOL_DESCRIPTION,
        title: 'Session-Attached Job',
        inputSchema: sessionJobInputSchema,
    }, async (args: SessionJobToolArgs) => {
        logger.debug('[hapiMCP] session_job:', args.action, args.jobKey);
        const result = await handleSessionJobTool(args, client.sessionId);
        return {
            content: [{ type: 'text' as const, text: result.text }],
            isError: result.isError,
        };
    });

    if (!peerToolsDisabled) {
        for (const name of Object.keys(peerTools) as (keyof typeof peerTools)[]) {
            registerTool(name, peerTools[name], (args: unknown) => {
                const metadata = name === 'spawn_peer' ? client.getMetadata() : null;
                return handlePeerTool(name, args, {
                    callerSessionId: client.sessionId,
                    name: metadata?.name,
                    agentSessionId: toSessionSummaryMetadata(metadata)?.agentSessionId,
                    cwd: resolveMcpSpawnPeerCwd({
                        workingDirectory,
                        skillWorkingDirectory: skillLookup?.workingDirectory,
                        sessionPath: metadata?.path,
                    }),
                });
            });
        }
    }

    if (skillLookup) {
        registerTool('skill_lookup', {
            description: 'Load a HAPI skill by exact name. When a user message starts with $name, call this tool with that name before acting.',
            title: 'Look Up Skill',
            inputSchema: skillLookupInputSchema,
        }, async (args: { name: string }) => {
            logger.debug('[hapiMCP] Looking up skill:', args.name);
            try {
                const skill = await resolveSkill(args.name, skillLookup.workingDirectory, {
                    flavor: skillLookup.flavor
                });
                if (!skill) {
                    throw new Error(`Skill not found: ${args.name}`);
                }

                const header = [
                    `Skill: ${skill.name}`,
                    ...(skill.description ? [`Description: ${skill.description}`] : [])
                ].join('\n');
                return {
                    content: [
                        {
                            type: 'text' as const,
                            text: `${header}\n\n${skill.body}`,
                        },
                    ],
                    isError: false,
                };
            } catch (error) {
                const message = error instanceof Error ? error.message : String(error);
                logger.debug('[hapiMCP] Failed to look up skill:', message);
                return {
                    content: [
                        {
                            type: 'text' as const,
                            text: `Failed to look up skill: ${message}`,
                        },
                    ],
                    isError: true,
                };
            }
        });
    }

    return mcp;
}

function readMcpSessionId(req: IncomingMessage): string | undefined {
    const raw = req.headers['mcp-session-id'];
    if (typeof raw === 'string') {
        return raw;
    }
    if (Array.isArray(raw)) {
        return raw[0];
    }
    return undefined;
}

export async function startHappyServer(client: ApiSessionClient, options: StartHappyServerOptions = {}) {
    const emitTitleSummary = options.emitTitleSummary ?? true;
    const enableChangeTitle = options.enableChangeTitle ?? true;
    const requestedTools = options.enabledTools ? new Set(options.enabledTools) : null;
    const includeTool = (name: HapiMcpToolName): boolean =>
        requestedTools === null || requestedTools.has(name);
    const peerToolsDisabled = isPeerToolsDisabled();
    const toolNames: HapiMcpToolName[] = [];
    if (enableChangeTitle && includeTool('change_title')) toolNames.push('change_title');
    for (const name of ['display_image', 'display_video', 'display_media'] as const) {
        if (includeTool(name)) toolNames.push(name);
    }
    if (!peerToolsDisabled) {
        for (const name of ['list_peers', 'ping_peer', 'inspect_peer', 'spawn_peer'] as const) {
            if (includeTool(name)) toolNames.push(name);
        }
    }
    if (includeTool(SESSION_JOB_TOOL_NAME)) toolNames.push(SESSION_JOB_TOOL_NAME);
    if (options.skillLookup && includeTool('skill_lookup')) toolNames.push('skill_lookup');
    if (toolNames.length === 0) {
        throw new Error('startHappyServer requires at least one enabled tool');
    }
    const enabledTools = new Set(toolNames);

    const transports = new Map<string, StreamableHTTPServerTransport>();
    const mcps = new Map<string, McpServer>();

    const createMcpTransport = () => {
        const mcp = createHapiMcpServer(
            client,
            emitTitleSummary,
            enableChangeTitle,
            options.skillLookup,
            options.workingDirectory,
            enabledTools,
            options.onChangeTitle
        );
        const transport = new StreamableHTTPServerTransport({
            sessionIdGenerator: () => randomUUID(),
            onsessioninitialized: (sessionId) => {
                transports.set(sessionId, transport);
                mcps.set(sessionId, mcp);
            },
            onsessionclosed: (sessionId) => {
                transports.delete(sessionId);
                const server = mcps.get(sessionId);
                mcps.delete(sessionId);
                void server?.close();
            },
        });
        void mcp.connect(transport);
        return transport;
    };

    const server = createServer(async (req, res) => {
        try {
            const sessionId = readMcpSessionId(req);
            const transport = sessionId
                ? transports.get(sessionId)
                : createMcpTransport();

            if (!transport) {
                if (!res.headersSent) {
                    res.writeHead(404).end();
                }
                return;
            }

            await transport.handleRequest(req, res);
        } catch (error) {
            logger.debug("Error handling request:", error);
            if (!res.headersSent) {
                res.writeHead(500).end();
            }
        }
    });

    const baseUrl = await new Promise<URL>((resolve) => {
        server.listen(0, "127.0.0.1", () => {
            const addr = server.address() as AddressInfo;
            resolve(new URL(`http://127.0.0.1:${addr.port}`));
        });
    });

    const mcpUrl = baseUrl.toString();
    client.updateMetadata((metadata) => ({
        ...metadata,
        hapiMcpUrl: mcpUrl,
    }));

    return {
        url: mcpUrl,
        toolNames,
        stop: () => {
            logger.debug('[hapiMCP] Stopping server');
            for (const mcp of mcps.values()) {
                mcp.close();
            }
            transports.clear();
            mcps.clear();
            server.close();
        }
    };
}
