# In-scope decomp holdouts

Functions in scope (no ARM7, no online area) that are not at the T2 finish line after the
2026-11-10 review rounds, including passes that read the tylzars/mkds matching decompilation for each one.
Most are fragments, empty stubs, or functions whose meaning the code (and the matching C) does not settle.

50 functions.

| Segment | Entry | Name | Area | Why |
|---|---|---|---|---|
| arm9_autoload1_itcm | 0x01FFF800 | item_update_held_record | code_item_1 | untyped pointer arg r0 |
| arm9_main | 0x02000477 | secure_area_filler_bytes | code_misc_1 | untyped pointer arg r0 |
| arm9_main | 0x02015894 | nns_g3d_probably_evaluates_a_keyframe_entry_at_02015894 | code_sys_gfx_1 | untyped pointer arg r0 |
| arm9_main | 0x0201E3C8 | effect_remove_list_node | code_effect_1 | untyped pointer arg r1 |
| arm9_main | 0x0201E494 | effect_push_list_head | code_effect_1 | untyped pointer arg r1 |
| arm9_main | 0x0201EB98 | getter_01eb98 | code_misc_1 | untyped pointer arg r0 |
| arm9_main | 0x020213A4 | sound_enqueue_command | code_sound_1 | untyped pointer arg r0 |
| arm9_main | 0x020219F0 | snd_get_next_inst_data | code_sound_1 | untyped pointer arg r0 |
| arm9_main | 0x02021F8C | relocation_021f8c | code_misc_1 | untyped pointer arg r0 |
| arm9_main | 0x020239A8 | g2d_blit_glyph_tiled | code_sys_gfx_1 | untyped pointer arg r1 |
| arm9_main | 0x02023BCC | g2d_blit_glyph_linear | code_sys_gfx_1 | untyped pointer arg r1 |
| arm9_main | 0x02025955 | g2d_setup_screen_vram_banks | code_sys_gfx_1 | untyped pointer arg r1 |
| arm9_main | 0x0204984C | scene_handle_wifi_error_maybe | code_menu_2 | name unconfirmed |
| arm9_main | 0x0204998C | scene_is_sleep_blocked_maybe | code_menu_2 | name unconfirmed |
| arm9_main | 0x0204F8A0 | mp_forward_stream_value_if_enabled_maybe | code_misc_1 | name unconfirmed |
| arm9_main | 0x02070398 | sub_arm9_main_02070398 | code_misc_2 | name unconfirmed |
| arm9_main | 0x02078C59 | sub_arm9_main_02078c59 | code_misc_2 | name unconfirmed |
| arm9_main | 0x02078D65 | sub_arm9_main_02078d65 | code_misc_2 | name unconfirmed |
| arm9_main | 0x020849F8 | gx_emit_object_draw_commands | code_sys_gfx_1 | untyped pointer arg r0 |
| arm9_main | 0x020B6DC4 | sub_arm9_main_020b6dc4 | code_misc_2 | name unconfirmed |
| arm9_main | 0x020BB5CC | sub_arm9_main_020bb5cc | code_misc_2 | name unconfirmed |
| arm9_main | 0x020CADF5 | menu_button_update_enabled_state_maybe | code_menu_3 | name unconfirmed |
| arm9_main | 0x020D879C | sub_arm9_main_020d879c | code_misc_3 | name unconfirmed |
| arm9_main | 0x020F1180 | sub_arm9_main_020f1180 | code_misc_3 | name unconfirmed |
| arm9_main | 0x020F11F0 | sub_arm9_main_020f11f0 | code_misc_3 | name unconfirmed |
| arm9_main | 0x020F1D70 | sub_arm9_main_020f1d70 | code_misc_3 | name unconfirmed |
| arm9_main | 0x020FCB4C | sub_arm9_main_020fcb4c | code_misc_3 | name unconfirmed |
| arm9_main | 0x0212F730 | sub_arm9_main_0212f730 | code_misc_3 | name unconfirmed |
| arm9_main | 0x0212F788 | sound_get_valid_record | code_sound_2 | untyped pointer arg r0 |
| ov9_000 | 0x02181695 | menu_init_network_dialog_resources | code_menu_6 | untyped pointer arg r0,r1 |
| ov9_000 | 0x021849D1 | sub_ov9_000_021849d1 | code_misc_4 | name unconfirmed |
| ov9_001 | 0x02181695 | obj_has_unflagged_record | code_obj_4 | untyped pointer arg r0,r1 |
| ov9_001 | 0x02189564 | obj_call_fn_ptr_189564 | code_obj_4 | untyped pointer arg r1 |
| ov9_001 | 0x0218AAE5 | mission_enemy_init_spawn_action | code_mission_1 | untyped pointer arg r1 |
| ov9_001 | 0x0219114D | mission_enemy_begin_hand_return_19114c | code_mission_1 | untyped pointer arg r1 |
| ov9_001 | 0x02199E1D | menu_check_record_type | code_menu_8 | untyped pointer arg r0 |
| ov9_001 | 0x0219BAD5 | menu_destroy_choice_screen | code_menu_8 | untyped pointer arg r0 |
| ov9_001 | 0x0219C3CD | menu_handle_file_list_input | code_menu_8 | untyped pointer arg r1 |
| ov9_001 | 0x021A0171 | obj_dispatch_selected_callback | code_obj_4 | untyped pointer arg r0 |
| ov9_001 | 0x021A1099 | menu_init_screen_state | code_menu_8 | untyped pointer arg r0 |
| ov9_001 | 0x021A1819 | menu_release_screen_resource_1a1818 | code_menu_9 | untyped pointer arg r2 |
| ov9_001 | 0x021A2379 | emblem_get_image_buffer_maybe | code_misc_4 | name unconfirmed; untyped pointer arg r1 |
| ov9_001 | 0x021A2969 | menu_restore_display_and_flush | code_menu_9 | untyped pointer arg r1 |
| ov9_003 | 0x021C50B9 | menu_cancel_prompt_maybe | code_menu_10 | name unconfirmed |
| ov9_003 | 0x021C5291 | menu_check_prompt_cancel_maybe | code_menu_10 | name unconfirmed |
| ov9_003 | 0x021C66FD | menu_close_dialog_on_choice_zero_maybe_ov9_003_021c66fd | code_menu_10 | name unconfirmed |
| ov9_003 | 0x021C6EFD | menu_close_dialog_on_choice_zero_maybe_ov9_003_021c6efd | code_menu_10 | name unconfirmed |
| ov9_003 | 0x021CC4AD | menu_handle_network_event_maybe | code_menu_11 | name unconfirmed |
| ov9_003 | 0x021D3449 | task_destroy | code_misc_5 | untyped pointer arg r1 |
| arm9_main | 0x0214F5E0 | list_push_front_arm9_main_0214f5e0 | code_sys_mem_2 | untyped pointer arg r1 |
