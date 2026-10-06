# Generated interrupt stubs for spacekernel (see idt.rs). Vectors with a CPU error
# code do not push the dummy 0. Every stub ends up in isr_common, which builds a
# TrapFrame (arch/x86_64/trap.rs) and calls x86_64_trap_handler(frame).
.section .text
.global isr_common
isr_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    cld
    mov rdi, rsp
    call x86_64_trap_handler
.global isr_return
isr_return:
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    iretq

.global isr_0
isr_0:
    push 0
    push 0
    jmp isr_common
.global isr_1
isr_1:
    push 0
    push 1
    jmp isr_common
.global isr_2
isr_2:
    push 0
    push 2
    jmp isr_common
.global isr_3
isr_3:
    push 0
    push 3
    jmp isr_common
.global isr_4
isr_4:
    push 0
    push 4
    jmp isr_common
.global isr_5
isr_5:
    push 0
    push 5
    jmp isr_common
.global isr_6
isr_6:
    push 0
    push 6
    jmp isr_common
.global isr_7
isr_7:
    push 0
    push 7
    jmp isr_common
.global isr_8
isr_8:
    push 8
    jmp isr_common
.global isr_9
isr_9:
    push 0
    push 9
    jmp isr_common
.global isr_10
isr_10:
    push 10
    jmp isr_common
.global isr_11
isr_11:
    push 11
    jmp isr_common
.global isr_12
isr_12:
    push 12
    jmp isr_common
.global isr_13
isr_13:
    push 13
    jmp isr_common
.global isr_14
isr_14:
    push 14
    jmp isr_common
.global isr_15
isr_15:
    push 0
    push 15
    jmp isr_common
.global isr_16
isr_16:
    push 0
    push 16
    jmp isr_common
.global isr_17
isr_17:
    push 17
    jmp isr_common
.global isr_18
isr_18:
    push 0
    push 18
    jmp isr_common
.global isr_19
isr_19:
    push 0
    push 19
    jmp isr_common
.global isr_20
isr_20:
    push 0
    push 20
    jmp isr_common
.global isr_21
isr_21:
    push 21
    jmp isr_common
.global isr_22
isr_22:
    push 0
    push 22
    jmp isr_common
.global isr_23
isr_23:
    push 0
    push 23
    jmp isr_common
.global isr_24
isr_24:
    push 0
    push 24
    jmp isr_common
.global isr_25
isr_25:
    push 0
    push 25
    jmp isr_common
.global isr_26
isr_26:
    push 0
    push 26
    jmp isr_common
.global isr_27
isr_27:
    push 0
    push 27
    jmp isr_common
.global isr_28
isr_28:
    push 0
    push 28
    jmp isr_common
.global isr_29
isr_29:
    push 29
    jmp isr_common
.global isr_30
isr_30:
    push 30
    jmp isr_common
.global isr_31
isr_31:
    push 0
    push 31
    jmp isr_common
.global isr_32
isr_32:
    push 0
    push 32
    jmp isr_common
.global isr_33
isr_33:
    push 0
    push 33
    jmp isr_common
.global isr_34
isr_34:
    push 0
    push 34
    jmp isr_common
.global isr_35
isr_35:
    push 0
    push 35
    jmp isr_common
.global isr_36
isr_36:
    push 0
    push 36
    jmp isr_common
.global isr_37
isr_37:
    push 0
    push 37
    jmp isr_common
.global isr_38
isr_38:
    push 0
    push 38
    jmp isr_common
.global isr_39
isr_39:
    push 0
    push 39
    jmp isr_common
.global isr_40
isr_40:
    push 0
    push 40
    jmp isr_common
.global isr_41
isr_41:
    push 0
    push 41
    jmp isr_common
.global isr_42
isr_42:
    push 0
    push 42
    jmp isr_common
.global isr_43
isr_43:
    push 0
    push 43
    jmp isr_common
.global isr_44
isr_44:
    push 0
    push 44
    jmp isr_common
.global isr_45
isr_45:
    push 0
    push 45
    jmp isr_common
.global isr_46
isr_46:
    push 0
    push 46
    jmp isr_common
.global isr_47
isr_47:
    push 0
    push 47
    jmp isr_common
.global isr_48
isr_48:
    push 0
    push 48
    jmp isr_common
.global isr_49
isr_49:
    push 0
    push 49
    jmp isr_common
.global isr_50
isr_50:
    push 0
    push 50
    jmp isr_common
.global isr_51
isr_51:
    push 0
    push 51
    jmp isr_common
.global isr_52
isr_52:
    push 0
    push 52
    jmp isr_common
.global isr_53
isr_53:
    push 0
    push 53
    jmp isr_common
.global isr_54
isr_54:
    push 0
    push 54
    jmp isr_common
.global isr_55
isr_55:
    push 0
    push 55
    jmp isr_common
.global isr_56
isr_56:
    push 0
    push 56
    jmp isr_common
.global isr_57
isr_57:
    push 0
    push 57
    jmp isr_common
.global isr_58
isr_58:
    push 0
    push 58
    jmp isr_common
.global isr_59
isr_59:
    push 0
    push 59
    jmp isr_common
.global isr_60
isr_60:
    push 0
    push 60
    jmp isr_common
.global isr_61
isr_61:
    push 0
    push 61
    jmp isr_common
.global isr_62
isr_62:
    push 0
    push 62
    jmp isr_common
.global isr_63
isr_63:
    push 0
    push 63
    jmp isr_common
.global isr_64
isr_64:
    push 0
    push 64
    jmp isr_common
.global isr_65
isr_65:
    push 0
    push 65
    jmp isr_common
.global isr_66
isr_66:
    push 0
    push 66
    jmp isr_common
.global isr_67
isr_67:
    push 0
    push 67
    jmp isr_common
.global isr_68
isr_68:
    push 0
    push 68
    jmp isr_common
.global isr_69
isr_69:
    push 0
    push 69
    jmp isr_common
.global isr_70
isr_70:
    push 0
    push 70
    jmp isr_common
.global isr_71
isr_71:
    push 0
    push 71
    jmp isr_common
.global isr_72
isr_72:
    push 0
    push 72
    jmp isr_common
.global isr_73
isr_73:
    push 0
    push 73
    jmp isr_common
.global isr_74
isr_74:
    push 0
    push 74
    jmp isr_common
.global isr_75
isr_75:
    push 0
    push 75
    jmp isr_common
.global isr_76
isr_76:
    push 0
    push 76
    jmp isr_common
.global isr_77
isr_77:
    push 0
    push 77
    jmp isr_common
.global isr_78
isr_78:
    push 0
    push 78
    jmp isr_common
.global isr_79
isr_79:
    push 0
    push 79
    jmp isr_common
.global isr_80
isr_80:
    push 0
    push 80
    jmp isr_common
.global isr_81
isr_81:
    push 0
    push 81
    jmp isr_common
.global isr_82
isr_82:
    push 0
    push 82
    jmp isr_common
.global isr_83
isr_83:
    push 0
    push 83
    jmp isr_common
.global isr_84
isr_84:
    push 0
    push 84
    jmp isr_common
.global isr_85
isr_85:
    push 0
    push 85
    jmp isr_common
.global isr_86
isr_86:
    push 0
    push 86
    jmp isr_common
.global isr_87
isr_87:
    push 0
    push 87
    jmp isr_common
.global isr_88
isr_88:
    push 0
    push 88
    jmp isr_common
.global isr_89
isr_89:
    push 0
    push 89
    jmp isr_common
.global isr_90
isr_90:
    push 0
    push 90
    jmp isr_common
.global isr_91
isr_91:
    push 0
    push 91
    jmp isr_common
.global isr_92
isr_92:
    push 0
    push 92
    jmp isr_common
.global isr_93
isr_93:
    push 0
    push 93
    jmp isr_common
.global isr_94
isr_94:
    push 0
    push 94
    jmp isr_common
.global isr_95
isr_95:
    push 0
    push 95
    jmp isr_common
.global isr_96
isr_96:
    push 0
    push 96
    jmp isr_common
.global isr_97
isr_97:
    push 0
    push 97
    jmp isr_common
.global isr_98
isr_98:
    push 0
    push 98
    jmp isr_common
.global isr_99
isr_99:
    push 0
    push 99
    jmp isr_common
.global isr_100
isr_100:
    push 0
    push 100
    jmp isr_common
.global isr_101
isr_101:
    push 0
    push 101
    jmp isr_common
.global isr_102
isr_102:
    push 0
    push 102
    jmp isr_common
.global isr_103
isr_103:
    push 0
    push 103
    jmp isr_common
.global isr_104
isr_104:
    push 0
    push 104
    jmp isr_common
.global isr_105
isr_105:
    push 0
    push 105
    jmp isr_common
.global isr_106
isr_106:
    push 0
    push 106
    jmp isr_common
.global isr_107
isr_107:
    push 0
    push 107
    jmp isr_common
.global isr_108
isr_108:
    push 0
    push 108
    jmp isr_common
.global isr_109
isr_109:
    push 0
    push 109
    jmp isr_common
.global isr_110
isr_110:
    push 0
    push 110
    jmp isr_common
.global isr_111
isr_111:
    push 0
    push 111
    jmp isr_common
.global isr_112
isr_112:
    push 0
    push 112
    jmp isr_common
.global isr_113
isr_113:
    push 0
    push 113
    jmp isr_common
.global isr_114
isr_114:
    push 0
    push 114
    jmp isr_common
.global isr_115
isr_115:
    push 0
    push 115
    jmp isr_common
.global isr_116
isr_116:
    push 0
    push 116
    jmp isr_common
.global isr_117
isr_117:
    push 0
    push 117
    jmp isr_common
.global isr_118
isr_118:
    push 0
    push 118
    jmp isr_common
.global isr_119
isr_119:
    push 0
    push 119
    jmp isr_common
.global isr_120
isr_120:
    push 0
    push 120
    jmp isr_common
.global isr_121
isr_121:
    push 0
    push 121
    jmp isr_common
.global isr_122
isr_122:
    push 0
    push 122
    jmp isr_common
.global isr_123
isr_123:
    push 0
    push 123
    jmp isr_common
.global isr_124
isr_124:
    push 0
    push 124
    jmp isr_common
.global isr_125
isr_125:
    push 0
    push 125
    jmp isr_common
.global isr_126
isr_126:
    push 0
    push 126
    jmp isr_common
.global isr_127
isr_127:
    push 0
    push 127
    jmp isr_common
.global isr_128
isr_128:
    push 0
    push 128
    jmp isr_common
.global isr_129
isr_129:
    push 0
    push 129
    jmp isr_common
.global isr_130
isr_130:
    push 0
    push 130
    jmp isr_common
.global isr_131
isr_131:
    push 0
    push 131
    jmp isr_common
.global isr_132
isr_132:
    push 0
    push 132
    jmp isr_common
.global isr_133
isr_133:
    push 0
    push 133
    jmp isr_common
.global isr_134
isr_134:
    push 0
    push 134
    jmp isr_common
.global isr_135
isr_135:
    push 0
    push 135
    jmp isr_common
.global isr_136
isr_136:
    push 0
    push 136
    jmp isr_common
.global isr_137
isr_137:
    push 0
    push 137
    jmp isr_common
.global isr_138
isr_138:
    push 0
    push 138
    jmp isr_common
.global isr_139
isr_139:
    push 0
    push 139
    jmp isr_common
.global isr_140
isr_140:
    push 0
    push 140
    jmp isr_common
.global isr_141
isr_141:
    push 0
    push 141
    jmp isr_common
.global isr_142
isr_142:
    push 0
    push 142
    jmp isr_common
.global isr_143
isr_143:
    push 0
    push 143
    jmp isr_common
.global isr_144
isr_144:
    push 0
    push 144
    jmp isr_common
.global isr_145
isr_145:
    push 0
    push 145
    jmp isr_common
.global isr_146
isr_146:
    push 0
    push 146
    jmp isr_common
.global isr_147
isr_147:
    push 0
    push 147
    jmp isr_common
.global isr_148
isr_148:
    push 0
    push 148
    jmp isr_common
.global isr_149
isr_149:
    push 0
    push 149
    jmp isr_common
.global isr_150
isr_150:
    push 0
    push 150
    jmp isr_common
.global isr_151
isr_151:
    push 0
    push 151
    jmp isr_common
.global isr_152
isr_152:
    push 0
    push 152
    jmp isr_common
.global isr_153
isr_153:
    push 0
    push 153
    jmp isr_common
.global isr_154
isr_154:
    push 0
    push 154
    jmp isr_common
.global isr_155
isr_155:
    push 0
    push 155
    jmp isr_common
.global isr_156
isr_156:
    push 0
    push 156
    jmp isr_common
.global isr_157
isr_157:
    push 0
    push 157
    jmp isr_common
.global isr_158
isr_158:
    push 0
    push 158
    jmp isr_common
.global isr_159
isr_159:
    push 0
    push 159
    jmp isr_common
.global isr_160
isr_160:
    push 0
    push 160
    jmp isr_common
.global isr_161
isr_161:
    push 0
    push 161
    jmp isr_common
.global isr_162
isr_162:
    push 0
    push 162
    jmp isr_common
.global isr_163
isr_163:
    push 0
    push 163
    jmp isr_common
.global isr_164
isr_164:
    push 0
    push 164
    jmp isr_common
.global isr_165
isr_165:
    push 0
    push 165
    jmp isr_common
.global isr_166
isr_166:
    push 0
    push 166
    jmp isr_common
.global isr_167
isr_167:
    push 0
    push 167
    jmp isr_common
.global isr_168
isr_168:
    push 0
    push 168
    jmp isr_common
.global isr_169
isr_169:
    push 0
    push 169
    jmp isr_common
.global isr_170
isr_170:
    push 0
    push 170
    jmp isr_common
.global isr_171
isr_171:
    push 0
    push 171
    jmp isr_common
.global isr_172
isr_172:
    push 0
    push 172
    jmp isr_common
.global isr_173
isr_173:
    push 0
    push 173
    jmp isr_common
.global isr_174
isr_174:
    push 0
    push 174
    jmp isr_common
.global isr_175
isr_175:
    push 0
    push 175
    jmp isr_common
.global isr_176
isr_176:
    push 0
    push 176
    jmp isr_common
.global isr_177
isr_177:
    push 0
    push 177
    jmp isr_common
.global isr_178
isr_178:
    push 0
    push 178
    jmp isr_common
.global isr_179
isr_179:
    push 0
    push 179
    jmp isr_common
.global isr_180
isr_180:
    push 0
    push 180
    jmp isr_common
.global isr_181
isr_181:
    push 0
    push 181
    jmp isr_common
.global isr_182
isr_182:
    push 0
    push 182
    jmp isr_common
.global isr_183
isr_183:
    push 0
    push 183
    jmp isr_common
.global isr_184
isr_184:
    push 0
    push 184
    jmp isr_common
.global isr_185
isr_185:
    push 0
    push 185
    jmp isr_common
.global isr_186
isr_186:
    push 0
    push 186
    jmp isr_common
.global isr_187
isr_187:
    push 0
    push 187
    jmp isr_common
.global isr_188
isr_188:
    push 0
    push 188
    jmp isr_common
.global isr_189
isr_189:
    push 0
    push 189
    jmp isr_common
.global isr_190
isr_190:
    push 0
    push 190
    jmp isr_common
.global isr_191
isr_191:
    push 0
    push 191
    jmp isr_common
.global isr_192
isr_192:
    push 0
    push 192
    jmp isr_common
.global isr_193
isr_193:
    push 0
    push 193
    jmp isr_common
.global isr_194
isr_194:
    push 0
    push 194
    jmp isr_common
.global isr_195
isr_195:
    push 0
    push 195
    jmp isr_common
.global isr_196
isr_196:
    push 0
    push 196
    jmp isr_common
.global isr_197
isr_197:
    push 0
    push 197
    jmp isr_common
.global isr_198
isr_198:
    push 0
    push 198
    jmp isr_common
.global isr_199
isr_199:
    push 0
    push 199
    jmp isr_common
.global isr_200
isr_200:
    push 0
    push 200
    jmp isr_common
.global isr_201
isr_201:
    push 0
    push 201
    jmp isr_common
.global isr_202
isr_202:
    push 0
    push 202
    jmp isr_common
.global isr_203
isr_203:
    push 0
    push 203
    jmp isr_common
.global isr_204
isr_204:
    push 0
    push 204
    jmp isr_common
.global isr_205
isr_205:
    push 0
    push 205
    jmp isr_common
.global isr_206
isr_206:
    push 0
    push 206
    jmp isr_common
.global isr_207
isr_207:
    push 0
    push 207
    jmp isr_common
.global isr_208
isr_208:
    push 0
    push 208
    jmp isr_common
.global isr_209
isr_209:
    push 0
    push 209
    jmp isr_common
.global isr_210
isr_210:
    push 0
    push 210
    jmp isr_common
.global isr_211
isr_211:
    push 0
    push 211
    jmp isr_common
.global isr_212
isr_212:
    push 0
    push 212
    jmp isr_common
.global isr_213
isr_213:
    push 0
    push 213
    jmp isr_common
.global isr_214
isr_214:
    push 0
    push 214
    jmp isr_common
.global isr_215
isr_215:
    push 0
    push 215
    jmp isr_common
.global isr_216
isr_216:
    push 0
    push 216
    jmp isr_common
.global isr_217
isr_217:
    push 0
    push 217
    jmp isr_common
.global isr_218
isr_218:
    push 0
    push 218
    jmp isr_common
.global isr_219
isr_219:
    push 0
    push 219
    jmp isr_common
.global isr_220
isr_220:
    push 0
    push 220
    jmp isr_common
.global isr_221
isr_221:
    push 0
    push 221
    jmp isr_common
.global isr_222
isr_222:
    push 0
    push 222
    jmp isr_common
.global isr_223
isr_223:
    push 0
    push 223
    jmp isr_common
.global isr_224
isr_224:
    push 0
    push 224
    jmp isr_common
.global isr_225
isr_225:
    push 0
    push 225
    jmp isr_common
.global isr_226
isr_226:
    push 0
    push 226
    jmp isr_common
.global isr_227
isr_227:
    push 0
    push 227
    jmp isr_common
.global isr_228
isr_228:
    push 0
    push 228
    jmp isr_common
.global isr_229
isr_229:
    push 0
    push 229
    jmp isr_common
.global isr_230
isr_230:
    push 0
    push 230
    jmp isr_common
.global isr_231
isr_231:
    push 0
    push 231
    jmp isr_common
.global isr_232
isr_232:
    push 0
    push 232
    jmp isr_common
.global isr_233
isr_233:
    push 0
    push 233
    jmp isr_common
.global isr_234
isr_234:
    push 0
    push 234
    jmp isr_common
.global isr_235
isr_235:
    push 0
    push 235
    jmp isr_common
.global isr_236
isr_236:
    push 0
    push 236
    jmp isr_common
.global isr_237
isr_237:
    push 0
    push 237
    jmp isr_common
.global isr_238
isr_238:
    push 0
    push 238
    jmp isr_common
.global isr_239
isr_239:
    push 0
    push 239
    jmp isr_common
.global isr_240
isr_240:
    push 0
    push 240
    jmp isr_common
.global isr_241
isr_241:
    push 0
    push 241
    jmp isr_common
.global isr_242
isr_242:
    push 0
    push 242
    jmp isr_common
.global isr_243
isr_243:
    push 0
    push 243
    jmp isr_common
.global isr_244
isr_244:
    push 0
    push 244
    jmp isr_common
.global isr_245
isr_245:
    push 0
    push 245
    jmp isr_common
.global isr_246
isr_246:
    push 0
    push 246
    jmp isr_common
.global isr_247
isr_247:
    push 0
    push 247
    jmp isr_common
.global isr_248
isr_248:
    push 0
    push 248
    jmp isr_common
.global isr_249
isr_249:
    push 0
    push 249
    jmp isr_common
.global isr_250
isr_250:
    push 0
    push 250
    jmp isr_common
.global isr_251
isr_251:
    push 0
    push 251
    jmp isr_common
.global isr_252
isr_252:
    push 0
    push 252
    jmp isr_common
.global isr_253
isr_253:
    push 0
    push 253
    jmp isr_common
.global isr_254
isr_254:
    push 0
    push 254
    jmp isr_common
.global isr_255
isr_255:
    push 0
    push 255
    jmp isr_common

.section .rodata
.balign 8
.global ISR_TABLE
ISR_TABLE:
    .quad isr_0
    .quad isr_1
    .quad isr_2
    .quad isr_3
    .quad isr_4
    .quad isr_5
    .quad isr_6
    .quad isr_7
    .quad isr_8
    .quad isr_9
    .quad isr_10
    .quad isr_11
    .quad isr_12
    .quad isr_13
    .quad isr_14
    .quad isr_15
    .quad isr_16
    .quad isr_17
    .quad isr_18
    .quad isr_19
    .quad isr_20
    .quad isr_21
    .quad isr_22
    .quad isr_23
    .quad isr_24
    .quad isr_25
    .quad isr_26
    .quad isr_27
    .quad isr_28
    .quad isr_29
    .quad isr_30
    .quad isr_31
    .quad isr_32
    .quad isr_33
    .quad isr_34
    .quad isr_35
    .quad isr_36
    .quad isr_37
    .quad isr_38
    .quad isr_39
    .quad isr_40
    .quad isr_41
    .quad isr_42
    .quad isr_43
    .quad isr_44
    .quad isr_45
    .quad isr_46
    .quad isr_47
    .quad isr_48
    .quad isr_49
    .quad isr_50
    .quad isr_51
    .quad isr_52
    .quad isr_53
    .quad isr_54
    .quad isr_55
    .quad isr_56
    .quad isr_57
    .quad isr_58
    .quad isr_59
    .quad isr_60
    .quad isr_61
    .quad isr_62
    .quad isr_63
    .quad isr_64
    .quad isr_65
    .quad isr_66
    .quad isr_67
    .quad isr_68
    .quad isr_69
    .quad isr_70
    .quad isr_71
    .quad isr_72
    .quad isr_73
    .quad isr_74
    .quad isr_75
    .quad isr_76
    .quad isr_77
    .quad isr_78
    .quad isr_79
    .quad isr_80
    .quad isr_81
    .quad isr_82
    .quad isr_83
    .quad isr_84
    .quad isr_85
    .quad isr_86
    .quad isr_87
    .quad isr_88
    .quad isr_89
    .quad isr_90
    .quad isr_91
    .quad isr_92
    .quad isr_93
    .quad isr_94
    .quad isr_95
    .quad isr_96
    .quad isr_97
    .quad isr_98
    .quad isr_99
    .quad isr_100
    .quad isr_101
    .quad isr_102
    .quad isr_103
    .quad isr_104
    .quad isr_105
    .quad isr_106
    .quad isr_107
    .quad isr_108
    .quad isr_109
    .quad isr_110
    .quad isr_111
    .quad isr_112
    .quad isr_113
    .quad isr_114
    .quad isr_115
    .quad isr_116
    .quad isr_117
    .quad isr_118
    .quad isr_119
    .quad isr_120
    .quad isr_121
    .quad isr_122
    .quad isr_123
    .quad isr_124
    .quad isr_125
    .quad isr_126
    .quad isr_127
    .quad isr_128
    .quad isr_129
    .quad isr_130
    .quad isr_131
    .quad isr_132
    .quad isr_133
    .quad isr_134
    .quad isr_135
    .quad isr_136
    .quad isr_137
    .quad isr_138
    .quad isr_139
    .quad isr_140
    .quad isr_141
    .quad isr_142
    .quad isr_143
    .quad isr_144
    .quad isr_145
    .quad isr_146
    .quad isr_147
    .quad isr_148
    .quad isr_149
    .quad isr_150
    .quad isr_151
    .quad isr_152
    .quad isr_153
    .quad isr_154
    .quad isr_155
    .quad isr_156
    .quad isr_157
    .quad isr_158
    .quad isr_159
    .quad isr_160
    .quad isr_161
    .quad isr_162
    .quad isr_163
    .quad isr_164
    .quad isr_165
    .quad isr_166
    .quad isr_167
    .quad isr_168
    .quad isr_169
    .quad isr_170
    .quad isr_171
    .quad isr_172
    .quad isr_173
    .quad isr_174
    .quad isr_175
    .quad isr_176
    .quad isr_177
    .quad isr_178
    .quad isr_179
    .quad isr_180
    .quad isr_181
    .quad isr_182
    .quad isr_183
    .quad isr_184
    .quad isr_185
    .quad isr_186
    .quad isr_187
    .quad isr_188
    .quad isr_189
    .quad isr_190
    .quad isr_191
    .quad isr_192
    .quad isr_193
    .quad isr_194
    .quad isr_195
    .quad isr_196
    .quad isr_197
    .quad isr_198
    .quad isr_199
    .quad isr_200
    .quad isr_201
    .quad isr_202
    .quad isr_203
    .quad isr_204
    .quad isr_205
    .quad isr_206
    .quad isr_207
    .quad isr_208
    .quad isr_209
    .quad isr_210
    .quad isr_211
    .quad isr_212
    .quad isr_213
    .quad isr_214
    .quad isr_215
    .quad isr_216
    .quad isr_217
    .quad isr_218
    .quad isr_219
    .quad isr_220
    .quad isr_221
    .quad isr_222
    .quad isr_223
    .quad isr_224
    .quad isr_225
    .quad isr_226
    .quad isr_227
    .quad isr_228
    .quad isr_229
    .quad isr_230
    .quad isr_231
    .quad isr_232
    .quad isr_233
    .quad isr_234
    .quad isr_235
    .quad isr_236
    .quad isr_237
    .quad isr_238
    .quad isr_239
    .quad isr_240
    .quad isr_241
    .quad isr_242
    .quad isr_243
    .quad isr_244
    .quad isr_245
    .quad isr_246
    .quad isr_247
    .quad isr_248
    .quad isr_249
    .quad isr_250
    .quad isr_251
    .quad isr_252
    .quad isr_253
    .quad isr_254
    .quad isr_255
