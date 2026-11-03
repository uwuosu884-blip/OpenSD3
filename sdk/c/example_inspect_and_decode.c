/*
 * example_inspect_and_decode.c — Minimal C99 Example for `opensd3.h`
 *
 * Build:
 *   gcc -O2 -Wall -Wextra sdk/c/example_inspect_and_decode.c -o example_inspect
 *
 * Run:
 *   ./example_inspect /path/to/SL-Death/Sounds/soundb12.obw [optional_first_hit.wav]
 *   ./example_inspect /path/to/sample.ttpw [optional_out.wav]
 */

#define OPENSD3_IMPLEMENTATION
#include "opensd3.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void write_u16le(FILE *fp, uint16_t v) {
    uint8_t b[2] = { (uint8_t)(v & 0xff), (uint8_t)(v >> 8) };
    fwrite(b, 1, 2, fp);
}

static void write_u32le(FILE *fp, uint32_t v) {
    uint8_t b[4] = {
        (uint8_t)(v & 0xff),
        (uint8_t)((v >> 8) & 0xff),
        (uint8_t)((v >> 16) & 0xff),
        (uint8_t)((v >> 24) & 0xff)
    };
    fwrite(b, 1, 4, fp);
}

static int write_stereo_f32_wav(const char *path, uint32_t sr, const float *l, const float *r, size_t frames) {
    FILE *fp = fopen(path, "wb");
    if (!fp) return -1;
    uint32_t data_bytes = (uint32_t)(frames * 2u * sizeof(float));
    fwrite("RIFF", 1, 4, fp);
    write_u32le(fp, 36u + data_bytes);
    fwrite("WAVEfmt ", 1, 8, fp);
    write_u32le(fp, 16u);
    write_u16le(fp, 3u); /* IEEE Float */
    write_u16le(fp, 2u); /* Stereo */
    write_u32le(fp, sr);
    write_u32le(fp, sr * 8u);
    write_u16le(fp, 8u);
    write_u16le(fp, 32u);
    fwrite("data", 1, 4, fp);
    write_u32le(fp, data_bytes);
    for (size_t i = 0; i < frames; ++i) {
        fwrite(&l[i], sizeof(float), 1, fp);
        fwrite(&r[i], sizeof(float), 1, fp);
    }
    fclose(fp);
    return 0;
}

static int decode_ttpw_slice_to_wav(const uint8_t *buf, size_t len, const char *wav_out) {
    os3_ttpw_header hdr;
    os3_status st = os3_parse_ttpw_header(buf, len, &hdr);
    if (st != OS3_OK) {
        fprintf(stderr, "os3_parse_ttpw_header failed (%d)\n", st);
        return 1;
    }
    printf("ttpw: %u ch | %u Hz | %u-bit | %u frames | %u sections\n",
           hdr.channels, hdr.sample_rate, hdr.bits_per_sample,
           hdr.total_frames, hdr.num_sections);

    if (!wav_out) return 0;

    float *l = (float *)calloc(hdr.total_frames, sizeof(float));
    float *r = (float *)calloc(hdr.total_frames, sizeof(float));
    if (!l || !r) {
        free(l);
        free(r);
        return 1;
    }
    int64_t got = os3_decode_ttpw_range_stereo_f32(buf, len, &hdr, 0, hdr.total_frames, l, r);
    if (got < 0) {
        fprintf(stderr, "decode failed (%lld)\n", (long long)got);
        free(l);
        free(r);
        return 1;
    }
    write_stereo_f32_wav(wav_out, hdr.sample_rate, l, r, (size_t)got);
    printf("Wrote %lld stereo frames -> %s\n", (long long)got, wav_out);
    free(l);
    free(r);
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s <soundb*.obw | *.oidx | *.ttpw> [out.wav]\n", argv[0]);
        return 1;
    }
    const char *in_path = argv[1];
    const char *wav_out = (argc >= 3) ? argv[2] : NULL;

    FILE *fp = fopen(in_path, "rb");
    if (!fp) {
        perror(in_path);
        return 1;
    }
    fseek(fp, 0, SEEK_END);
    long fsize_long = ftell(fp);
    fseek(fp, 0, SEEK_SET);
    if (fsize_long <= 0) {
        fclose(fp);
        return 1;
    }
    uint64_t full_size = (uint64_t)fsize_long;

    /* Read up to 4 MB header prefix (enough for any .oidx or single .ttpw) */
    size_t head_cap = full_size < (4u * 1024u * 1024u) ? (size_t)full_size : (4u * 1024u * 1024u);
    uint8_t *head = (uint8_t *)malloc(head_cap);
    size_t nread = fread(head, 1, head_cap, fp);

    if (os3_looks_like_ttpw(head, nread)) {
        fclose(fp);
        int rc = decode_ttpw_slice_to_wav(head, nread, wav_out);
        free(head);
        return rc;
    }

    os3_ttbw_index idx;
    os3_status st;
    if (os3_looks_like_ttbw(head, nread)) {
        st = os3_parse_ttbw_index(head, nread, full_size, &idx);
    } else {
        st = os3_parse_oidx_payload(head, nread, 0, &idx);
    }
    free(head);

    if (st != OS3_OK) {
        fprintf(stderr, "Failed to parse %s (status=%d)\n", in_path, st);
        fclose(fp);
        return 1;
    }

    printf("Parsed %s: %zu mics | %zu groups | %zu samples | xor_mask=0x%08x\n",
           in_path, idx.mic_count, idx.group_count, idx.total_records, idx.xor_mask);
    for (size_t m = 0; m < idx.mic_count; ++m) {
        printf("  Mic[%zu] = %s\n", m, idx.mics[m]);
    }
    for (size_t g = 0; g < idx.group_count; ++g) {
        printf("  Group %-22s (%zu records)\n", idx.groups[g].name, idx.groups[g].record_count);
    }

    int rc = 0;
    if (wav_out && idx.data_payload_offset > 0 && idx.group_count > 0 && idx.groups[0].record_count > 0) {
        const os3_sample_record *rec = &idx.groups[0].records[0];
        uint64_t abs_off = idx.data_payload_offset + (uint64_t)rec->data_offset;
        uint8_t *slice = (uint8_t *)malloc(rec->byte_size);
        if (slice && fseek(fp, (long)abs_off, SEEK_SET) == 0
            && fread(slice, 1, rec->byte_size, fp) == rec->byte_size) {
            rc = decode_ttpw_slice_to_wav(slice, rec->byte_size, wav_out);
        }
        free(slice);
    }

    os3_free_ttbw_index(&idx);
    fclose(fp);
    return rc;
}
