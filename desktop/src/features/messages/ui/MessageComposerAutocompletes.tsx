import { setKeepMentionedAgentsPinned } from "@/features/messages/lib/autoPinMentionedAgentsPreference";
import type {
  ChannelSuggestion,
  UseChannelLinksResult,
} from "@/features/messages/lib/useChannelLinks";
import type {
  EmojiSuggestion,
  UseEmojiAutocompleteResult,
} from "@/features/messages/lib/useEmojiAutocomplete";
import type { UseMentionsResult } from "@/features/messages/lib/useMentions";
import { ChannelAutocomplete } from "./ChannelAutocomplete";
import { EmojiAutocomplete } from "./EmojiAutocomplete";
import {
  MentionAutocomplete,
  type MentionSuggestion,
} from "./MentionAutocomplete";

type MessageComposerAutocompletesProps = {
  /**
   * Whether the mention menu offers its agent-audience controls. Edit
   * composers and channels without a persistent audience omit them.
   */
  audienceControlsEnabled: boolean;
  channelLinks: UseChannelLinksResult;
  emojiAutocomplete: UseEmojiAutocompleteResult;
  isEditorFocused: boolean;
  keepMentionedAgentsPinned: boolean;
  lockedAgentPubkeys: ReadonlySet<string>;
  mentions: UseMentionsResult;
  onChannelSelect: (suggestion: ChannelSuggestion) => void;
  onEmojiSelect: (suggestion: EmojiSuggestion) => void;
  onMentionSelect: (suggestion: MentionSuggestion) => void;
  onToggleAlwaysAddressAgent: (suggestion: MentionSuggestion) => void;
};

/**
 * The message composer's three suggestion overlays. Each one gates its own
 * rendering on `isEditorFocused`, so a background composer replaying a stale
 * update cannot resurrect a suggestion menu over the focused composer.
 */
export function MessageComposerAutocompletes({
  audienceControlsEnabled,
  channelLinks,
  emojiAutocomplete,
  isEditorFocused,
  keepMentionedAgentsPinned,
  lockedAgentPubkeys,
  mentions,
  onChannelSelect,
  onEmojiSelect,
  onMentionSelect,
  onToggleAlwaysAddressAgent,
}: MessageComposerAutocompletesProps) {
  return (
    <>
      <EmojiAutocomplete
        isEditorFocused={isEditorFocused}
        onSelect={onEmojiSelect}
        selectedIndex={emojiAutocomplete.emojiSelectedIndex}
        suggestions={
          emojiAutocomplete.isEmojiAutocompleteOpen
            ? emojiAutocomplete.emojiSuggestions
            : []
        }
      />
      <ChannelAutocomplete
        isEditorFocused={isEditorFocused}
        onSelect={onChannelSelect}
        selectedIndex={channelLinks.channelSelectedIndex}
        suggestions={
          channelLinks.isChannelOpen ? channelLinks.channelSuggestions : []
        }
      />
      <MentionAutocomplete
        isEditorFocused={isEditorFocused}
        keepMentionedAgentsPinned={keepMentionedAgentsPinned}
        lockedAgentPubkeys={lockedAgentPubkeys}
        onKeepMentionedAgentsPinnedChange={
          audienceControlsEnabled ? setKeepMentionedAgentsPinned : undefined
        }
        onToggleAlwaysAddressAgent={
          audienceControlsEnabled ? onToggleAlwaysAddressAgent : undefined
        }
        onFetchMore={mentions.fetchMoreSuggestions}
        onDismiss={mentions.cancelMentionAutocomplete}
        onSelect={onMentionSelect}
        selectedIndex={mentions.mentionSelectedIndex}
        suggestions={mentions.isMentionOpen ? mentions.suggestions : []}
      />
    </>
  );
}
