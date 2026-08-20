/* Test hooks the mock camera provides; not part of the vendor SDK. */
#ifndef SVB_MOCK_HOOKS_H
#define SVB_MOCK_HOOKS_H
void SVB_mock_reset(void);
void SVB_mock_unplug(void);
void SVB_mock_replug(void);
void SVB_mock_freeze(int frozen);
void SVB_mock_fail_next_control(void);
void SVB_mock_set_dropped(int dropped);
#endif
